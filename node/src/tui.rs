//! A node on screen: who it is connected to, what it holds, the order book as it hears it, the
//! settlements it has seen and a line to type wants into. One terminal per node is the demo.
use std::io::IsTerminal;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::config::Tokens;
use crate::console;
use crate::node::{self, Closed, Command, Handle, View};

/// Whether there is a terminal to draw on.
pub fn available() -> bool {
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// Draws the node until the operator quits, and returns what it last showed.
pub async fn run(mut handle: Handle, tokens: Tokens) -> Result<View> {
    let mut terminal = ratatui::init();
    let mut keys = EventStream::new();
    let mut input = String::new();
    // What the last command got back, shown under the input line until the next one.
    let mut notice = format!("try: {}", console::HELP);
    let mut redraw = tokio::time::interval(Duration::from_secs(1));

    let result: Result<()> = async {
        loop {
            let view = handle.view.borrow_and_update().clone();
            terminal.draw(|frame| draw(frame, &view, &tokens, &input, &notice))?;
            tokio::select! {
                changed = handle.view.changed() => if changed.is_err() { break },
                _ = redraw.tick() => {}
                Some(Ok(Event::Key(key))) = keys.next() => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Esc => break,
                        KeyCode::Char(c) => input.push(c),
                        KeyCode::Backspace => { input.pop(); }
                        KeyCode::Enter if !input.trim().is_empty() => {
                            match console::parse(&input, &tokens) {
                                Ok(Command::Quit) => break,
                                Ok(command) => {
                                    handle.commands.send(command).await.ok();
                                    notice.clear();
                                }
                                Err(e) => notice = e.to_string(),
                            }
                            input.clear();
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    ratatui::restore();
    let last = handle.view.borrow().clone();
    handle.commands.send(Command::Quit).await.ok();
    handle.task.await.ok();
    result.map(|()| last)
}

fn short(account: &str) -> String {
    format!("{}…", node::short(account))
}

fn titled(title: String) -> Block<'static> {
    Block::bordered()
        .title(Span::styled(title, Style::new().add_modifier(Modifier::BOLD)))
        .border_style(Style::new().fg(Color::DarkGray))
}

/// The last lines that fit, so that the newest is always in view.
fn tail<'a>(lines: Vec<Line<'a>>, area: Rect) -> Vec<Line<'a>> {
    let room = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(room);
    lines.into_iter().skip(skip).collect()
}

pub fn draw(frame: &mut Frame, view: &View, tokens: &Tokens, input: &str, notice: &str) {
    let dim = Style::new().fg(Color::DarkGray);
    let good = Style::new().fg(Color::Green);
    let mine = Style::new().fg(Color::Cyan);

    // Side by side when there is room; stacked in a narrow pane, as when three nodes share a screen.
    let wide = frame.area().width >= 100;
    let [header, top, wants, middle, log, prompt] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(if wide { 7 } else { 5 }),
        Constraint::Length(if wide { 0 } else { 5 }),
        Constraint::Min(if wide { 6 } else { 10 }),
        Constraint::Min(6),
        Constraint::Length(3),
    ])
    .areas(frame.area());
    let (peers, balances, wants) = match wide {
        true => {
            let [peers, balances, wants] =
                Layout::horizontal([Constraint::Length(16), Constraint::Length(24), Constraint::Min(30)]).areas(top);
            (peers, balances, wants)
        }
        false => {
            let [peers, balances] = Layout::horizontal([Constraint::Length(16), Constraint::Min(20)]).areas(top);
            (peers, balances, wants)
        }
    };
    let [book, settled] = match wide {
        true => Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(middle),
        false => Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(middle),
    };

    // Who this is and where it settles.
    let fee = match view.fee_pips {
        Some(pips) => format!("fee {}%", pips as f64 / 10_000.0),
        None => "reading the verifier…".into(),
    };
    let account = match wide {
        true => view.account.clone(),
        false => short(&view.account),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                " Gossip Market ",
                Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
            Span::styled(format!("  {}", view.name), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("  account {account}")),
        ]),
        Line::styled(
            match wide {
                true => format!(
                    " settles through {} at {} · {fee} · listening on {}",
                    view.verifier,
                    view.rpc,
                    view.listen.first().map_or("…", String::as_str)
                ),
                false => format!(" settles through {} · {fee}", view.verifier),
            },
            dim,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), header);

    let peer_lines: Vec<Line> = match view.peers.is_empty() {
        true => vec![Line::styled("nobody yet", dim)],
        false => view.peers.iter().map(|peer| Line::raw(short(peer))).collect(),
    };
    frame.render_widget(
        Paragraph::new(tail(peer_lines, peers)).block(titled(format!(" Peers ({}) ", view.peers.len()))),
        peers,
    );

    let balance_lines: Vec<Line> = view
        .balances
        .iter()
        .map(|(token, amount)| Line::raw(tokens.show(token, *amount)))
        .collect();
    frame.render_widget(
        Paragraph::new(tail(balance_lines, balances)).block(titled(" In the verifier ".into())),
        balances,
    );

    let want_lines: Vec<Line> = match view.wants.is_empty() {
        true => vec![Line::styled("none: type a want below", dim)],
        false => view
            .wants
            .iter()
            .map(|want| {
                let offer = format!(
                    "#{} {} for ≥ {}",
                    want.id,
                    tokens.show(&want.give, want.remaining),
                    tokens.show(&want.take, want.take_for_remaining)
                );
                match (want.closed, &want.locked) {
                    (Some(Closed::Filled), _) => Line::styled(format!("#{} filled", want.id), dim),
                    (Some(Closed::Cancelled), _) => Line::styled(format!("#{} cancelled", want.id), dim),
                    (None, Some(ring)) => {
                        Line::from(vec![Span::raw(offer), Span::styled(format!("  in ring {ring}"), mine)])
                    }
                    (None, None) => Line::from(vec![Span::raw(offer), Span::styled("  open", good)]),
                }
            })
            .collect(),
    };
    frame.render_widget(
        Paragraph::new(tail(want_lines, wants)).block(titled(" My wants ".into())),
        wants,
    );

    let book_lines: Vec<Line> = match view.book.is_empty() {
        true => vec![Line::styled("no open wants heard", dim)],
        false => view
            .book
            .iter()
            .map(|want| {
                let text = format!(
                    "{}#{}  {} for ≥ {}",
                    short(&want.maker),
                    want.id,
                    tokens.show(&want.give, want.give_amount),
                    tokens.show(&want.take, want.take_amount)
                );
                match want.maker == view.account {
                    true => Line::from(vec![Span::raw(text), Span::styled("  (me)", mine)]),
                    false => Line::raw(text),
                }
            })
            .collect(),
    };
    frame.render_widget(
        Paragraph::new(tail(book_lines, book)).block(titled(format!(" Order book, as heard ({}) ", view.book.len()))),
        book,
    );

    let settled_lines: Vec<Line> = match view.settlements.is_empty() {
        true => vec![Line::styled("nothing settled yet", dim)],
        false => view
            .settlements
            .iter()
            .flat_map(|settlement| {
                // "mine": this node's own nonce was seen spent on chain. "heard": a notice only.
                let who = match settlement.mine {
                    true => Span::styled(" · mine", good),
                    false => Span::styled(" · heard", dim),
                };
                let head = Line::from(vec![
                    Span::raw(format!(
                        "{} ring {} · {}-party",
                        settlement.at.with_timezone(&chrono::Local).format("%H:%M:%S"),
                        settlement.ring.id(),
                        settlement.ring.legs.len()
                    )),
                    who,
                ]);
                let tx = settlement
                    .tx
                    .as_deref()
                    .unwrap_or("settled by another maker; hash not heard");
                [head, Line::styled(format!("  tx {tx}"), dim)]
            })
            .collect(),
    };
    frame.render_widget(
        Paragraph::new(tail(settled_lines, settled))
            .block(titled(format!(" Settlements ({}) ", view.settlements.len()))),
        settled,
    );

    let log_lines: Vec<Line> = view
        .log
        .iter()
        .map(|(_, line)| match line.contains("SETTLED") {
            true => Line::styled(line.clone(), good),
            false => Line::raw(line.clone()),
        })
        .collect();
    frame.render_widget(Paragraph::new(tail(log_lines, log)).block(titled(" Log ".into())), log);

    let typed = Line::from(vec![Span::styled("> ", mine), Span::raw(input.to_owned())]);
    let hint = match notice.is_empty() {
        true => " Enter to send · Esc to quit ".to_owned(),
        false => format!(" {notice} "),
    };
    frame.render_widget(
        Paragraph::new(typed).block(
            Block::bordered()
                .title_bottom(Span::styled(hint, dim))
                .border_style(dim),
        ),
        prompt,
    );
    frame.set_cursor_position((prompt.x + 3 + input.chars().count() as u16, prompt.y + 1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TokenConfig;
    use crate::node::{Settlement, WantView};
    use market::{Leg, Ring, Want};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::BTreeMap;

    fn screen(view: &View, input: &str) -> String {
        screen_of(view, input, 120, 34)
    }

    fn screen_of(view: &View, input: &str, width: u16, height: u16) -> String {
        let token = |id: &str, decimals| TokenConfig {
            id: id.into(),
            decimals,
        };
        let tokens = Tokens::from(BTreeMap::from([
            ("USDC".to_owned(), token("nep141:usdc", 6)),
            ("CREDITS".to_owned(), token("nep141:credits", 3)),
        ]));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, view, &tokens, input, "try: want …"))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    // The screen is the demo. This draws one mid-trade and checks that what a viewer needs to
    // follow is on it; run with `--nocapture` to look at it.
    #[test]
    fn shows_what_a_viewer_needs_to_follow_a_trade() {
        let me = "a".repeat(64);
        let other = "b".repeat(64);
        let leg = |maker: &str, give: &str, take: &str| Leg {
            maker: maker.into(),
            want_id: 1,
            want_rev: 0,
            give: give.into(),
            give_amount: 10_000_000,
            take: take.into(),
            take_amount: 99_990,
        };
        let ring = Ring {
            legs: vec![
                leg(&me, "nep141:usdc", "nep141:credits"),
                leg(&other, "nep141:credits", "nep141:usdc"),
            ],
        };
        let view = View {
            name: "alice".into(),
            account: me.clone(),
            peer_id: "12D3KooWAlice".into(),
            listen: vec!["/ip4/192.168.1.7/tcp/4001".into()],
            verifier: "intents.devnet".into(),
            rpc: "http://127.0.0.1:3030".into(),
            fee_pips: Some(100),
            peers: vec![other.clone()],
            balances: vec![("nep141:usdc".into(), 40_000_000), ("nep141:credits".into(), 99_990)],
            wants: vec![WantView {
                id: 2,
                rev: 0,
                give: "nep141:usdc".into(),
                remaining: 5_000_000,
                take: "nep141:credits".into(),
                take_for_remaining: 47_500,
                locked: Some(ring.id()),
                closed: None,
            }],
            book: vec![Want {
                maker: other.clone(),
                id: 3,
                rev: 0,
                give: "nep141:credits".into(),
                give_amount: 50_000,
                take: "nep141:usdc".into(),
                take_amount: 4_900_000,
                expires_ms: 0,
                addrs: vec![],
            }],
            settlements: vec![Settlement {
                at: chrono::Utc::now(),
                ring: ring.clone(),
                tx: Some("9GtGG11soq8F3SDcnAdZdKzLLztmiFMkTUxiahVfzvZ6".into()),
                submitter: Some(other.clone()),
                mine: true,
            }],
            holding: vec![],
            log: [(1, "12:00:01 SETTLED ring".to_owned())].into(),
        };
        let screen = screen(&view, "want 5 USDC for 47.5 CREDITS");
        println!("{screen}");
        for expected in [
            "Gossip Market",
            "alice",
            "settles through intents.devnet",
            "fee 0.01%",
            "Peers (1)",
            "40 USDC",
            "99.99 CREDITS",
            "#2 5 USDC for ≥ 47.5 CREDITS",
            &format!("in ring {}", ring.id()),
            "bbbbbbbb…#3  50 CREDITS for ≥ 4.9 USDC",
            "tx 9GtGG11soq8F3SDcnAdZdKzLLztmiFMkTUxiahVfzvZ6",
            "2-party · mine",
            "> want 5 USDC for 47.5 CREDITS",
        ] {
            assert!(screen.contains(expected), "the screen lacks `{expected}`");
        }
    }

    // Three nodes side by side on one screen is how the demo is filmed: a third of a terminal
    // each. Everything a viewer follows must survive that width.
    #[test]
    fn fits_a_third_of_a_screen() {
        let mut view = View {
            name: "bob".into(),
            account: "b".repeat(64),
            verifier: "intents.devnet".into(),
            fee_pips: Some(100),
            balances: vec![("nep141:usdc".into(), 40_000_000)],
            ..View::default()
        };
        view.wants.push(WantView {
            id: 1,
            rev: 0,
            give: "nep141:usdc".into(),
            remaining: 10_000_000,
            take: "nep141:credits".into(),
            take_for_remaining: 95_000,
            locked: None,
            closed: None,
        });
        view.wants.push(WantView {
            closed: Some(Closed::Filled),
            id: 2,
            ..view.wants[0].clone()
        });
        let screen = screen_of(&view, "", 66, 40);
        println!("{screen}");
        for expected in [
            "bob",
            "account bbbbbbbb…",
            "settles through intents.devnet · fee 0.01%",
            "40 USDC",
            "#1 10 USDC for ≥ 95 CREDITS  open",
            "#2 filled",
            "Order book",
            "Settlements",
            "Log",
        ] {
            assert!(screen.contains(expected), "the screen lacks `{expected}`");
        }
    }

    #[test]
    fn an_empty_node_says_what_to_do() {
        let screen = screen(&View::default(), "");
        for expected in [
            "nobody yet",
            "none: type a want below",
            "no open wants heard",
            "nothing settled yet",
        ] {
            assert!(screen.contains(expected), "the screen lacks `{expected}`");
        }
    }
}

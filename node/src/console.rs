//! What an operator types at a running node, and the node without a screen: log lines on standard
//! output, commands on standard input.
use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::Tokens;
use crate::node::{Command, Handle};

pub const HELP: &str = "want 10 USDC for 95 CREDITS · cancel 1 · quit";

/// `want 10 USDC for 95 CREDITS`, `cancel 2`, `quit`.
pub fn parse(line: &str, tokens: &Tokens) -> Result<Command> {
    let line = line.trim();
    let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    match verb.to_ascii_lowercase().as_str() {
        "want" => {
            let (give, take) = rest
                .split_once(" for ")
                .ok_or_else(|| anyhow!("say what for: want 10 USDC for 95 CREDITS"))?;
            Ok(Command::Want {
                give: tokens.parse(give)?,
                take: tokens.parse(take)?,
            })
        }
        "cancel" => {
            let id = rest.trim().trim_start_matches('#');
            Ok(Command::Cancel(
                id.parse().map_err(|_| anyhow!("which want? cancel 2"))?,
            ))
        }
        "quit" | "q" | "exit" => Ok(Command::Quit),
        _ => bail!("try: {HELP}"),
    }
}

/// Runs until the node stops or the operator quits.
pub async fn run(mut handle: Handle, tokens: Tokens) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    let mut printed = 0;
    {
        let view = handle.view.borrow();
        println!("{} is account {} and peer {}", view.name, view.account, view.peer_id);
        println!("type, for example: {HELP}");
    }
    loop {
        for (seq, line) in handle.view.borrow_and_update().log.iter() {
            if *seq > printed {
                println!("{line}");
                printed = *seq;
            }
        }
        tokio::select! {
            changed = handle.view.changed() => if changed.is_err() { break },
            line = lines.next_line(), if stdin_open => match line {
                Ok(Some(line)) if line.trim().is_empty() => {}
                Ok(Some(line)) => match parse(&line, &tokens) {
                    Ok(Command::Quit) => break,
                    Ok(command) => { handle.commands.send(command).await.ok(); }
                    Err(e) => println!("{e}"),
                },
                // Started without a terminal: keep running, there is just nobody to type.
                _ => stdin_open = false,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    handle.commands.send(Command::Quit).await.ok();
    handle.task.await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TokenConfig;
    use std::collections::BTreeMap;

    fn tokens() -> Tokens {
        let token = |id: &str, decimals| TokenConfig {
            id: id.into(),
            decimals,
        };
        Tokens::from(BTreeMap::from([
            ("USDC".to_owned(), token("nep141:usdc.near", 6)),
            ("CREDITS".to_owned(), token("nep141:credits.near", 3)),
        ]))
    }

    // Typed live, in front of people. A want read the wrong way round sells at the wrong price.
    #[test]
    fn reads_a_want_the_way_it_is_said() {
        let Ok(Command::Want { give, take }) = parse("want 10 USDC for 95.5 credits", &tokens()) else {
            panic!("not a want")
        };
        assert_eq!(give, ("nep141:usdc.near".into(), 10_000_000));
        assert_eq!(take, ("nep141:credits.near".into(), 95_500));

        assert!(matches!(parse(" cancel #2 ", &tokens()), Ok(Command::Cancel(2))));
        assert!(matches!(parse("quit", &tokens()), Ok(Command::Quit)));
        for wrong in ["want 10 USDC", "want 10 USDC for 5 DOGE", "cancel", "buy 10 USDC", ""] {
            assert!(parse(wrong, &tokens()).is_err(), "{wrong}");
        }
    }
}

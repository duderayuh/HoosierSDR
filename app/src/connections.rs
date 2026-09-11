//! Connections: where Tripwires send, set up once and picked by name.
//!
//! Telegram used to mean pasting a bot token, then hunting down a numeric
//! chat id (and, for a forum group, a topic id) and typing it into every
//! rule. Here the token is checked (`getMe` — the bot's name comes back),
//! and the chats the bot can see are *discovered*: `getUpdates` lists the
//! recent messages the bot received, and each one names its chat, and — in
//! a forum group — its topic. The listener names the ones they want as
//! **destinations**: "Me", "ECPR team › Arrests", "ECPR team › Stroke".
//! Several destinations may share one chat with different topics.
//!
//! A bot in a group with privacy mode on (the default) only receives
//! commands, replies to it, and mentions — so the setup copy asks for a
//! `/start@bot` in each topic, which always arrives. `getUpdates` refuses
//! while a webhook is set on the token; manual entry stays as the fallback.
//! Telegram returns at most 100 pending updates a call, oldest first, so
//! discovery pages through them with `offset` — which acknowledges them.
//! Nothing else reads this bot's updates, and every chat seen is kept in
//! `known_chats`, so nothing is lost by draining the queue.

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

/// A named place a message can go: a chat, and optionally a forum topic in
/// it (`topic_id` blank = the chat itself, or a forum's General topic).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Destination {
    pub id: String,
    pub name: String,
    pub chat_id: String,
    #[serde(default)]
    pub topic_id: String,
}

impl Destination {
    /// `chat` or `chat:topic`, the form every send helper takes.
    pub fn target(&self) -> String {
        crate::alerts::join_destination(&self.chat_id, &self.topic_id)
    }
}

/// A forum topic seen in a chat.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Topic {
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

/// A chat the bot has heard from.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct KnownChat {
    pub id: String,
    /// `private` | `group` | `supergroup` | `channel`
    #[serde(default)]
    pub kind: String,
    /// The group's title, or the person's name for a private chat.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub is_forum: bool,
    #[serde(default)]
    pub topics: Vec<Topic>,
    /// When a message from it was last seen (epoch seconds).
    #[serde(default)]
    pub seen: i64,
}

/// The chats (and forum topics) in a `getUpdates` reply, newest knowledge
/// winning. Pure, so it is tested without the network.
pub fn parse_updates(v: &serde_json::Value) -> Vec<KnownChat> {
    let mut out: Vec<KnownChat> = Vec::new();
    let Some(list) = v["result"].as_array() else {
        return out;
    };
    for u in list {
        // Every place an update can carry a chat: a message of some kind, or
        // the bot being added to / removed from one.
        for key in [
            "message",
            "edited_message",
            "channel_post",
            "edited_channel_post",
            "my_chat_member",
            "chat_member",
        ] {
            let m = &u[key];
            if !m.is_object() {
                continue;
            }
            let chat = &m["chat"];
            let Some(id) = chat["id"]
                .as_i64()
                .map(|i| i.to_string())
                .or_else(|| chat["id"].as_str().map(str::to_string))
            else {
                continue;
            };
            let kind = chat["type"].as_str().unwrap_or("").to_string();
            let title = chat["title"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    let first = chat["first_name"].as_str().unwrap_or("");
                    let last = chat["last_name"].as_str().unwrap_or("");
                    let name = format!("{first} {last}").trim().to_string();
                    (!name.is_empty()).then_some(name)
                })
                .or_else(|| chat["username"].as_str().map(|u| format!("@{u}")))
                .unwrap_or_default();
            let is_forum = chat["is_forum"].as_bool().unwrap_or(false);
            let seen = m["date"].as_i64().unwrap_or(0);
            let entry = match out.iter_mut().position(|c| c.id == id) {
                Some(i) => &mut out[i],
                None => {
                    out.push(KnownChat {
                        id: id.clone(),
                        ..Default::default()
                    });
                    out.last_mut().unwrap()
                }
            };
            if !kind.is_empty() {
                entry.kind = kind;
            }
            if !title.is_empty() {
                entry.title = crate::analyzers::clean_line(&title, 80);
            }
            entry.is_forum |= is_forum;
            entry.seen = entry.seen.max(seen);
            // A topic: the message's thread, named by the topic's creation
            // message (a topic message replies to it), or by the creation /
            // rename service message itself.
            let in_topic = m["is_topic_message"].as_bool().unwrap_or(false)
                || m["forum_topic_created"].is_object();
            if let (true, Some(tid)) = (in_topic, m["message_thread_id"].as_i64()) {
                let name = m["forum_topic_created"]["name"]
                    .as_str()
                    .or_else(|| m["forum_topic_edited"]["name"].as_str())
                    .or_else(|| m["reply_to_message"]["forum_topic_created"]["name"].as_str())
                    .map(|n| crate::analyzers::clean_line(n, 80))
                    .unwrap_or_default();
                match entry.topics.iter_mut().find(|t| t.id == tid) {
                    Some(t) => {
                        if !name.is_empty() {
                            t.name = name;
                        }
                    }
                    None => entry.topics.push(Topic { id: tid, name }),
                }
                entry.is_forum = true;
            }
        }
    }
    out
}

/// The newest `update_id` in a `getUpdates` reply, to page past it.
pub fn last_update_id(v: &serde_json::Value) -> Option<i64> {
    v["result"]
        .as_array()?
        .iter()
        .filter_map(|u| u["update_id"].as_i64())
        .max()
}

/// Fold newly seen chats into the saved list: titles and topic names
/// refresh, nothing already known is forgotten (updates only reach back a
/// day, and a topic that went quiet is still a topic).
pub fn merge_chats(saved: &mut Vec<KnownChat>, fresh: Vec<KnownChat>) {
    for f in fresh {
        match saved.iter_mut().find(|c| c.id == f.id) {
            Some(c) => {
                if !f.title.is_empty() {
                    c.title = f.title;
                }
                if !f.kind.is_empty() {
                    c.kind = f.kind;
                }
                c.is_forum |= f.is_forum;
                c.seen = c.seen.max(f.seen);
                for t in f.topics {
                    match c.topics.iter_mut().find(|x| x.id == t.id) {
                        Some(x) => {
                            if !t.name.is_empty() {
                                x.name = t.name;
                            }
                        }
                        None => c.topics.push(t),
                    }
                }
            }
            None => saved.push(f),
        }
    }
    for c in saved.iter_mut() {
        c.topics.sort_by_key(|t| t.id);
    }
    saved.sort_by_key(|c| std::cmp::Reverse(c.seen));
    saved.truncate(100);
}

/// Tidy destinations the listener (or the phone) handed us: ids, names,
/// chat ids that look like chat ids, numeric topics, no duplicates.
pub fn sanitize_destinations(list: &mut Vec<Destination>) -> Result<(), String> {
    list.truncate(100);
    let mut seen = std::collections::HashSet::new();
    for (i, d) in list.iter_mut().enumerate() {
        d.id = crate::analyzers::clean_line(&d.id, 40);
        if d.id.is_empty()
            || !d
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            d.id = format!("d{}-{i}", crate::library::now());
        }
        d.name = crate::analyzers::clean_line(&d.name, 60);
        d.chat_id = d.chat_id.trim().chars().take(64).collect();
        if !d
            .chat_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '@'))
        {
            return Err(format!(
                "destination “{}”: the chat id has unexpected characters",
                d.name
            ));
        }
        d.topic_id = d
            .topic_id
            .trim()
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(16)
            .collect();
        if d.name.is_empty() {
            d.name = if d.topic_id.is_empty() {
                format!("Chat {}", d.chat_id)
            } else {
                format!("Chat {} › topic {}", d.chat_id, d.topic_id)
            };
        }
        if !seen.insert(d.id.clone()) {
            d.id = format!("{}-{i}", d.id);
        }
    }
    list.retain(|d| !d.chat_id.is_empty());
    Ok(())
}

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(timeout_secs)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// The error Telegram gave, readable.
fn tg_error(status: u16, text: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
    let d = v["description"].as_str().unwrap_or(text.trim());
    if status == 409 || d.to_ascii_lowercase().contains("webhook") {
        "this bot has a webhook set (another service receives its messages), so chats can't be discovered here — enter the chat id by hand".into()
    } else if status == 401 {
        "Telegram does not recognise this bot token".into()
    } else {
        format!("Telegram HTTP {status}: {d}")
    }
}

/// What `getMe` says about the saved bot.
#[derive(Serialize, Clone, Debug, Default)]
pub struct BotInfo {
    pub ok: bool,
    pub username: String,
    pub name: String,
    /// Privacy mode off: the bot sees every group message, not just
    /// commands and mentions.
    pub reads_all: bool,
    pub error: String,
}

#[tauri::command]
pub async fn telegram_verify() -> Result<BotInfo, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let url = match crate::alerts::telegram_api("getMe") {
            Ok(u) => u,
            Err(e) => {
                return Ok(BotInfo {
                    error: e,
                    ..Default::default()
                })
            }
        };
        let mut r = agent(15)
            .get(&url)
            .call()
            .map_err(|e| format!("telegram: {e}"))?;
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap_or_default();
        if status != 200 {
            return Ok(BotInfo {
                error: tg_error(status, &text),
                ..Default::default()
            });
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let me = &v["result"];
        Ok(BotInfo {
            ok: true,
            username: me["username"].as_str().unwrap_or("").to_string(),
            name: me["first_name"].as_str().unwrap_or("").to_string(),
            reads_all: me["can_read_all_group_messages"].as_bool().unwrap_or(false),
            error: String::new(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read the bot's recent updates, fold the chats and topics they name into
/// the saved list, and return it.
#[tauri::command]
pub async fn telegram_discover(app: AppHandle) -> Result<Vec<KnownChat>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let url = crate::alerts::telegram_api("getUpdates")?;
        // No `allowed_updates`: Telegram keeps that filter for the bot from
        // then on, and later features want other update kinds.
        let mut offset: Option<i64> = None;
        let mut fresh = Vec::new();
        for _ in 0..20 {
            let mut body = serde_json::json!({ "limit": 100, "timeout": 0 });
            if let Some(o) = offset {
                body["offset"] = o.into();
            }
            let mut r = agent(20)
                .post(&url)
                .header("Content-Type", "application/json")
                .send(body.to_string().as_bytes())
                .map_err(|e| format!("telegram: {e}"))?;
            let status = r.status().as_u16();
            let text = r.body_mut().read_to_string().unwrap_or_default();
            if status != 200 {
                return Err(tg_error(status, &text));
            }
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            let page = v["result"].as_array().map(|a| a.len()).unwrap_or(0);
            merge_chats(&mut fresh, parse_updates(&v));
            match last_update_id(&v) {
                Some(id) if page >= 100 => offset = Some(id + 1),
                _ => break,
            }
        }
        crate::alerts::update_known_chats(&app, fresh)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Send a short hello to one destination, so the listener sees it land.
#[tauri::command]
pub async fn telegram_test_destination(destination: Destination) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if destination.chat_id.trim().is_empty() {
            return Err("that destination has no chat id".to_string());
        }
        let text = format!("✅ HoosierSDR can reach “{}”.", destination.name);
        crate::alerts::send_text(&destination.target(), &text, 30)
            .map(|_| format!("sent to {}", destination.name))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn updates() -> serde_json::Value {
        serde_json::json!({ "ok": true, "result": [
            { "update_id": 1, "message": { "message_id": 5, "date": 100,
                "chat": { "id": 111, "type": "private", "first_name": "Sam", "last_name": "Rivera" },
                "text": "/start" } },
            { "update_id": 2, "my_chat_member": { "date": 110,
                "chat": { "id": -1001, "type": "supergroup", "title": "Test Team", "is_forum": true } } },
            { "update_id": 3, "message": { "message_id": 9, "date": 120, "message_thread_id": 57,
                "chat": { "id": -1001, "type": "supergroup", "title": "Test Team", "is_forum": true },
                "forum_topic_created": { "name": "Arrests", "icon_color": 7322096 } } },
            { "update_id": 4, "message": { "message_id": 12, "date": 130, "message_thread_id": 64, "is_topic_message": true,
                "chat": { "id": -1001, "type": "supergroup", "title": "Test Team", "is_forum": true },
                "reply_to_message": { "message_id": 11, "forum_topic_created": { "name": "Stroke" } },
                "text": "/start@hoosier_bot" } },
            { "update_id": 5, "message": { "message_id": 13, "date": 140,
                "chat": { "id": -1001, "type": "supergroup", "title": "Test Team", "is_forum": true },
                "text": "/start in General" } },
            { "update_id": 6, "channel_post": { "message_id": 1, "date": 150,
                "chat": { "id": -1002, "type": "channel", "title": "Feed" } } }
        ] })
    }

    #[test]
    fn updates_name_chats_and_forum_topics() {
        let chats = parse_updates(&updates());
        assert_eq!(chats.len(), 3);
        let me = chats.iter().find(|c| c.id == "111").unwrap();
        assert_eq!(
            (me.kind.as_str(), me.title.as_str()),
            ("private", "Sam Rivera")
        );
        let team = chats.iter().find(|c| c.id == "-1001").unwrap();
        assert!(team.is_forum);
        assert_eq!(team.title, "Test Team");
        assert_eq!(team.seen, 140);
        assert_eq!(
            team.topics,
            vec![
                Topic {
                    id: 57,
                    name: "Arrests".into()
                },
                Topic {
                    id: 64,
                    name: "Stroke".into()
                }
            ]
        );
        assert_eq!(
            chats.iter().find(|c| c.id == "-1002").unwrap().kind,
            "channel"
        );
    }

    #[test]
    fn paging_starts_after_the_newest_update() {
        assert_eq!(last_update_id(&updates()), Some(6));
        assert_eq!(
            last_update_id(&serde_json::json!({ "ok": true, "result": [] })),
            None
        );
    }

    #[test]
    fn merging_keeps_what_was_known() {
        let mut saved = vec![KnownChat {
            id: "-1001".into(),
            title: "Old title".into(),
            is_forum: true,
            topics: vec![
                Topic {
                    id: 99,
                    name: "Quiet topic".into(),
                },
                Topic {
                    id: 57,
                    name: String::new(),
                },
            ],
            seen: 50,
            ..Default::default()
        }];
        merge_chats(&mut saved, parse_updates(&updates()));
        let team = saved.iter().find(|c| c.id == "-1001").unwrap();
        assert_eq!(team.title, "Test Team");
        assert_eq!(team.topics.len(), 3, "{:?}", team.topics);
        assert_eq!(
            team.topics[0],
            Topic {
                id: 57,
                name: "Arrests".into()
            }
        );
        assert!(
            team.topics.iter().any(|t| t.id == 99),
            "a quiet topic is not forgotten"
        );
        assert_eq!(saved[0].id, "-1002", "most recently seen first");
    }

    #[test]
    fn destinations_are_tidied() {
        let mut list = vec![
            Destination {
                id: "a".into(),
                name: " ECPR › Arrests ".into(),
                chat_id: " -1001 ".into(),
                topic_id: "57x".into(),
            },
            Destination {
                id: "a".into(),
                name: "".into(),
                chat_id: "-1001".into(),
                topic_id: "".into(),
            },
            Destination {
                id: "../x".into(),
                name: "Nothing".into(),
                chat_id: "".into(),
                topic_id: "".into(),
            },
        ];
        sanitize_destinations(&mut list).unwrap();
        assert_eq!(list.len(), 2, "no chat, no destination");
        assert_eq!(list[0].name, "ECPR › Arrests");
        assert_eq!(list[0].target(), "-1001:57");
        assert_eq!(list[1].name, "Chat -1001");
        assert_ne!(list[0].id, list[1].id);
        let mut bad = vec![Destination {
            id: "b".into(),
            name: "x".into(),
            chat_id: "12; drop".into(),
            topic_id: "".into(),
        }];
        assert!(sanitize_destinations(&mut bad).is_err());
    }
}

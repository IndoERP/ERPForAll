// SPDX-License-Identifier: MIT OR Apache-2.0
//! Data for the full workspace and the header's quick chat. The caller's active company and
//! current membership are checked even for administrators.
use super::*;
use erp_sdk::serde_json::Value as Json;
use std::collections::BTreeSet;

fn company(env: &Env) -> Result<i64> {
    env.company_id()?
        .ok_or_else(|| Error::validation("Select a company for Discuss."))
}
fn bad_row() -> Error {
    Error::invariant("Invalid stored conversation summary.")
}
fn checked_query(value: &str) -> Result<&str> {
    if value.len() > 256 {
        return Err(Error::validation(
            "Search text may contain at most 256 bytes.",
        ));
    }
    Ok(value.trim())
}
pub(super) async fn readable(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    ids: Vec<i64>,
) -> Result<BTreeSet<i64>> {
    if ids.is_empty() || env.registry().model(model).is_none() {
        return Ok(BTreeSet::new());
    }
    let domain = Domain::condition("id", Operator::In, ids)?;
    match env.records(model)?.search(tx, &domain).await {
        Ok(found) => Ok(found.ids().into_iter().collect()),
        Err(err) if err.kind() == erp_sdk::ErrorKind::Access => Ok(BTreeSet::new()),
        Err(err) => Err(err),
    }
}
/// Convert a bounded prefix of sanitized chatter HTML to plain text for a list row.
fn preview(html: &str) -> String {
    let mut text = String::new();
    let mut tag = false;
    for ch in html.chars() {
        match ch {
            '<' => {
                tag = true;
                text.push(' ');
            }
            '>' => tag = false,
            _ if !tag => text.push(ch),
            _ => {}
        }
    }
    let text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}
fn parse_cursor(cursor: Option<&str>) -> Result<(i64, i64)> {
    let Some(cursor) = cursor else {
        return Ok((i64::MAX, i64::MAX));
    };
    let invalid = || Error::validation("Invalid conversation cursor.");
    let (last, id) = cursor.split_once(':').ok_or_else(invalid)?;
    let last: i64 = last.parse().map_err(|_| invalid())?;
    let id: i64 = id.parse().map_err(|_| invalid())?;
    if last < 0 || id < 1 {
        return Err(invalid());
    }
    Ok((last, id))
}

#[methods(DiscussChannel, addon = "mail")]
impl DiscussChannel {
    #[method]
    #[rpc(effect=read,model)]
    async fn conversation_people(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        query: String,
    ) -> Result<Value> {
        let env = this.env();
        internal(tx, env).await?;
        let q = checked_query(&query)?;
        if !tx
            .has_field_access(env, "res.partner", "name", erp_sdk::FieldOp::Read)
            .await?
        {
            return Ok(Value::Json(Arc::new(json!([]))));
        }
        let rows=tx.query("SELECT u.id::bigint,p.id::bigint,p.name::text
             FROM res_users u JOIN res_partner p ON p.id=u.partner_id JOIN res_company_users_rel co ON co.user_id=u.id
             WHERE u.active AND NOT u.share AND p.active AND co.cid=$1::bigint AND u.id<>$2::bigint
               AND p.name IS NOT NULL AND strpos(lower(p.name),lower($3::text))>0
             ORDER BY lower(p.name),u.id LIMIT 50",&[&company(env)?,&env.uid(),&q]).await?;
        let mut candidates = Vec::new();
        for r in rows {
            candidates.push((
                r.try_get::<_, i64>(0).map_err(|_| bad_row())?,
                r.try_get::<_, i64>(1).map_err(|_| bad_row())?,
                r.try_get::<_, String>(2).map_err(|_| bad_row())?,
            ));
        }
        let partners = readable(
            tx,
            env,
            "res.partner",
            candidates.iter().map(|v| v.1).collect(),
        )
        .await?;
        let result = candidates
            .into_iter()
            .filter(|v| partners.contains(&v.1))
            .take(25)
            .map(|(id, _, name)| json!({"id":id,"name":name}))
            .collect::<Vec<_>>();
        Ok(Value::Json(Arc::new(json!(result))))
    }

    #[method]
    #[rpc(effect=write,model)]
    async fn conversation_start(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        user_ids: Vec<i64>,
        name: Option<String>,
    ) -> Result<i64> {
        let env = this.env();
        internal(tx, env).await?;
        let company = company(env)?;
        if user_ids.is_empty() || user_ids.len() >= MAX_MEMBERS || user_ids.contains(&env.uid()) {
            return Err(Error::validation("Choose between 1 and 99 other people."));
        }
        let users: BTreeSet<_> = user_ids.into_iter().collect();
        let peers = users.iter().copied().collect::<Vec<_>>();
        let rows=tx.query("SELECT p.id::bigint FROM res_users u JOIN res_partner p ON p.id=u.partner_id
             JOIN res_company_users_rel co ON co.user_id=u.id WHERE u.id=ANY($1::bigint[]) AND co.cid=$2::bigint
             AND u.active AND NOT u.share AND p.active ORDER BY u.id",&[&peers,&company]).await?;
        if rows.len() != peers.len() {
            return Err(Error::validation(
                "Choose active people in the current company.",
            ));
        }
        let partners: Vec<i64> = rows
            .iter()
            .map(|r| r.try_get(0).map_err(|_| bad_row()))
            .collect::<Result<_>>()?;
        env.records("res.partner")?
            .browse(partners)?
            .check_access(tx, Operation::Read)
            .await?;
        let system = env.with_context_replaced(Context::new()).sudo(true);
        let channels = system.records("discuss.channel")?;
        let vals = RawVals::new().set_cell("company_id", Cell::Id(company));
        let channel = if peers.len() == 1 {
            let peer = peers[0];
            let uid = env.uid();
            let pair = vals
                .clone()
                .set_cell("first_user_id", Cell::Id(uid.min(peer)))
                .set_cell("second_user_id", Cell::Id(uid.max(peer)));
            let pairs = system.records("discuss.channel.pair")?;
            let result = pairs
                .upsert(tx, UpsertCall::new("_direct_pair").with_rows([pair]))
                .await?;
            let saved = result.rows().first().ok_or_else(bad_row)?;
            let pair_id = saved.id().get();
            if saved.outcome() != UpsertOutcome::Inserted {
                let rows = tx
                    .query(
                        "SELECT p.channel_id::bigint FROM discuss_channel_pair p
                     JOIN discuss_channel_member m ON m.channel_id=p.channel_id
                     WHERE p.id=$1::bigint AND m.user_id=$2::bigint",
                        &[&pair_id, &uid],
                    )
                    .await?;
                return rows
                    .first()
                    .ok_or_else(|| Error::access("Current conversation membership is required."))?
                    .try_get(0)
                    .map_err(|_| bad_row());
            }
            let row = vals
                .set_cell("name", Cell::Str("Direct message".into()))
                .set_cell("channel_type", Cell::Str("direct".into()));
            let id = channels.create(tx, vec![row]).await?.ids()[0];
            pairs
                .browse([pair_id])?
                .write(tx, RawVals::new().set_cell("channel_id", Cell::Id(id)))
                .await?;
            id
        } else {
            let name = name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or("Group conversation");
            if name.is_empty() || name.len() > 240 {
                return Err(Error::validation(
                    "Enter a group name of at most 240 bytes.",
                ));
            }
            channels
                .create(
                    tx,
                    vec![
                        vals.set_cell("name", Cell::Str(name.into()))
                            .set_cell("channel_type", Cell::Str("group".into())),
                    ],
                )
                .await?
                .ids()[0]
        };
        let mut members = vec![
            RawVals::new()
                .set_cell("channel_id", Cell::Id(channel))
                .set_cell("user_id", Cell::Id(env.uid()))
                .set_cell("is_owner", Cell::Bool(peers.len() > 1)),
        ];
        members.extend(peers.into_iter().map(|uid| {
            RawVals::new()
                .set_cell("channel_id", Cell::Id(channel))
                .set_cell("user_id", Cell::Id(uid))
                .set_cell("is_owner", Cell::Bool(false))
        }));
        env.with_context_replaced(Context::new())
            .sudo(true)
            .records("discuss.channel.member")?
            .create(tx, members)
            .await?;
        wake(tx).await?;
        Ok(channel)
    }

    #[method]
    #[rpc(effect=read,model)]
    async fn conversation_search(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        query: Option<String>,
        filter: Option<String>,
        cursor: Option<String>,
    ) -> Result<Value> {
        let (items, next) = summaries(
            tx,
            this.env(),
            None,
            query.as_deref().unwrap_or_default(),
            filter.as_deref().unwrap_or("all"),
            cursor.as_deref(),
        )
        .await?;
        Ok(Value::Json(Arc::new(json!({"items":items,"next":next}))))
    }

    #[method]
    #[rpc(effect=read)]
    async fn conversation_summary(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let (items, _) =
            summaries(tx, this.env(), Some(channel_id(this)?), "", "all", None).await?;
        Ok(Value::Json(Arc::new(
            items.into_iter().next().unwrap_or(Json::Null),
        )))
    }

    #[method]
    #[rpc(effect=read,model)]
    async fn conversation_saved(
        this: &RecordSet<DiscussChannel>,
        tx: &mut Transaction,
        cursor: Option<i64>,
    ) -> Result<Value> {
        let env = this.env();
        internal(tx, env).await?;
        if cursor.is_some_and(|v| v < 1) {
            return Err(Error::validation("Invalid saved message cursor."));
        }
        let rows = tx.query(
            "SELECT msg.id::bigint,c.id::bigint,c.name::text,left(msg.body::text,2048),peer.id::bigint,peer.name::text
             FROM res_users me JOIN mail_message_res_partner_bookmarked_rel b ON b.res_partner_id=me.partner_id
             JOIN mail_message msg ON msg.id=b.mail_message_id JOIN discuss_channel c ON msg.model='discuss.channel' AND msg.res_id=c.id
             JOIN discuss_channel_member m ON m.channel_id=c.id AND m.user_id=me.id
             LEFT JOIN discuss_channel_pair dp ON dp.channel_id=c.id
             LEFT JOIN res_users other ON other.id=CASE WHEN dp.first_user_id=me.id THEN dp.second_user_id
                   WHEN dp.second_user_id=me.id THEN dp.first_user_id END AND other.active AND NOT other.share
             LEFT JOIN res_company_users_rel co ON co.user_id=other.id AND co.cid=c.company_id
             LEFT JOIN res_partner peer ON peer.id=other.partner_id AND peer.active AND co.user_id IS NOT NULL
             WHERE me.id=$1::bigint AND c.company_id=$2::bigint AND msg.id<$3::bigint AND msg.message_type<>'user_notification'
             ORDER BY msg.id DESC LIMIT 51",
             &[&env.uid(), &company(env)?, &cursor.unwrap_or(i64::MAX)],
        ).await?;
        let mut items = Vec::new();
        let more = rows.len() > 50;
        for row in rows.into_iter().take(50) {
            items.push((
                row.try_get::<_, i64>(0).map_err(|_| bad_row())?,
                row.try_get::<_, i64>(1).map_err(|_| bad_row())?,
                row.try_get::<_, String>(2).map_err(|_| bad_row())?,
                row.try_get::<_, Option<String>>(3).map_err(|_| bad_row())?,
                row.try_get::<_, Option<i64>>(4).map_err(|_| bad_row())?,
                row.try_get::<_, Option<String>>(5).map_err(|_| bad_row())?,
            ));
        }
        let next = more.then(|| items.last().map(|r| r.0)).flatten();
        let chans = readable(
            tx,
            env,
            "discuss.channel",
            items.iter().map(|r| r.1).collect(),
        )
        .await?;
        let msgs = readable(tx, env, "mail.message", items.iter().map(|r| r.0).collect()).await?;
        let body = tx
            .has_field_access(env, "mail.message", "body", erp_sdk::FieldOp::Read)
            .await?;
        let peers = readable(
            tx,
            env,
            "res.partner",
            items.iter().filter_map(|item| item.4).collect(),
        )
        .await?;
        let names = tx
            .has_field_access(env, "res.partner", "name", erp_sdk::FieldOp::Read)
            .await?;
        let result = items
            .into_iter()
            .filter(|row| chans.contains(&row.1) && msgs.contains(&row.0))
            .map(|(id, channel, base_name, text, peer, peer_name)| {
                let name = if names && peer.is_some_and(|id| peers.contains(&id)) {
                    peer_name.unwrap_or(base_name)
                } else {
                    base_name
                };
                let snippet = if body {
                    preview(text.as_deref().unwrap_or_default())
                } else {
                    String::new()
                };
                json!({"id":id, "channel_id":channel, "channel_name":name, "preview":snippet})
            })
            .collect::<Vec<_>>();
        Ok(Value::Json(Arc::new(json!({"items":result,"next":next}))))
    }
}

struct Summary {
    id: i64,
    name: String,
    kind: String,
    seen: i64,
    owner: bool,
    latest: Option<i64>,
    unread: i64,
    bookmarks: i64,
    author: Option<i64>,
    sender: Option<String>,
    peer: Option<i64>,
    peer_name: Option<String>,
    date: Option<String>,
    body: Option<String>,
}

async fn summaries(
    tx: &mut Transaction,
    env: &Env,
    id: Option<i64>,
    query: &str,
    filter: &str,
    cursor: Option<&str>,
) -> Result<(Vec<Json>, Option<String>)> {
    internal(tx, env).await?;
    let query = checked_query(query)?;
    if !matches!(filter, "all" | "unread" | "saved") {
        return Err(Error::validation("Unknown conversation filter."));
    }
    let (after_activity, after_channel) = parse_cursor(cursor)?;
    // Limit membership candidates before loading bodies and counting indicators. The record rules
    // for messages, contacts and channels are reapplied in batches to that bounded page below.
    let rows=tx.query(r#"WITH page AS MATERIALIZED (
        SELECT c.id,c.name,c.channel_type,m.seen_message_id,m.is_owner,u.partner_id,latest.id AS latest_id,
               peer.id AS peer_partner,peer.name AS peer_name
          FROM discuss_channel_member m JOIN res_users u ON u.id=m.user_id JOIN discuss_channel c ON c.id=m.channel_id
          LEFT JOIN LATERAL (SELECT msg.id FROM mail_message msg
                WHERE msg.model='discuss.channel' AND msg.res_id=c.id AND msg.message_type<>'user_notification'
                ORDER BY msg.id DESC LIMIT 1) latest ON true
          LEFT JOIN LATERAL (SELECT p.id,p.name FROM discuss_channel_member other JOIN res_users ou ON ou.id=other.user_id
                JOIN res_partner p ON p.id=ou.partner_id JOIN res_company_users_rel co ON co.user_id=ou.id AND co.cid=c.company_id
                WHERE c.channel_type='direct' AND other.channel_id=c.id AND other.user_id<>u.id AND ou.active AND NOT ou.share AND p.active LIMIT 1) peer ON true
         WHERE m.user_id=$1::bigint AND c.company_id=$2::bigint AND ($3::bigint IS NULL OR c.id=$3::bigint)
           AND strpos(lower(COALESCE(peer.name,c.name)),lower($4::text))>0
           AND (COALESCE(latest.id,0),c.id)<($5::bigint,$6::bigint)
           AND ($7::text<>'unread' OR EXISTS (SELECT FROM mail_message msg WHERE msg.model='discuss.channel' AND msg.res_id=c.id
                AND msg.id>COALESCE(m.seen_message_id,0) AND msg.message_type<>'user_notification' AND msg.author_id IS DISTINCT FROM u.partner_id))
           AND ($7::text<>'saved' OR EXISTS (SELECT FROM mail_message_res_partner_bookmarked_rel b JOIN mail_message msg ON msg.id=b.mail_message_id
                WHERE b.res_partner_id=u.partner_id AND msg.model='discuss.channel' AND msg.res_id=c.id AND msg.message_type<>'user_notification'))
         ORDER BY COALESCE(latest.id,0) DESC,c.id DESC LIMIT 51)
        SELECT p.id::bigint,p.name::text,COALESCE(p.channel_type,'group')::text,COALESCE(p.seen_message_id,0)::bigint,p.is_owner,
               p.latest_id::bigint,p.peer_partner::bigint,p.peer_name::text,msg.author_id::bigint,a.name::text,
               to_char(msg.date,'YYYY-MM-DD"T"HH24:MI:SS"Z"'),left(msg.body::text,2048),
               (SELECT count(*) FROM (SELECT FROM mail_message x WHERE x.model='discuss.channel' AND x.res_id=p.id AND x.message_type<>'user_notification'
                 AND x.id>COALESCE(p.seen_message_id,0) AND x.author_id IS DISTINCT FROM p.partner_id LIMIT 1000) un),
               (SELECT count(*) FROM (SELECT FROM mail_message_res_partner_bookmarked_rel b JOIN mail_message x ON x.id=b.mail_message_id WHERE b.res_partner_id=p.partner_id
                 AND x.model='discuss.channel' AND x.res_id=p.id AND x.message_type<>'user_notification' LIMIT 1000) saved)
          FROM page p LEFT JOIN mail_message msg ON msg.id=p.latest_id LEFT JOIN res_partner a ON a.id=msg.author_id
         ORDER BY COALESCE(p.latest_id,0) DESC,p.id DESC"#,
         &[&env.uid(),&company(env)?,&id,&query,&after_activity,&after_channel,&filter]).await?;
    let more = rows.len() > 50;
    let mut page = Vec::new();
    for r in rows.into_iter().take(50) {
        page.push(Summary {
            id: r.try_get(0).map_err(|_| bad_row())?,
            name: r.try_get(1).map_err(|_| bad_row())?,
            kind: r.try_get(2).map_err(|_| bad_row())?,
            seen: r.try_get(3).map_err(|_| bad_row())?,
            owner: r.try_get(4).map_err(|_| bad_row())?,
            latest: r.try_get(5).map_err(|_| bad_row())?,
            peer: r.try_get(6).map_err(|_| bad_row())?,
            peer_name: r.try_get(7).map_err(|_| bad_row())?,
            author: r.try_get(8).map_err(|_| bad_row())?,
            sender: r.try_get(9).map_err(|_| bad_row())?,
            date: r.try_get(10).map_err(|_| bad_row())?,
            body: r.try_get(11).map_err(|_| bad_row())?,
            unread: r.try_get(12).map_err(|_| bad_row())?,
            bookmarks: r.try_get(13).map_err(|_| bad_row())?,
        });
    }
    let next = if more {
        page.last()
            .map(|r| format!("{}:{}", r.latest.unwrap_or(0), r.id))
    } else {
        None
    };
    if page.is_empty() {
        return Ok((vec![], next));
    }
    let channels = readable(
        tx,
        env,
        "discuss.channel",
        page.iter().map(|s| s.id).collect(),
    )
    .await?;
    let messages = readable(
        tx,
        env,
        "mail.message",
        page.iter().filter_map(|s| s.latest).collect(),
    )
    .await?;
    let partners = readable(
        tx,
        env,
        "res.partner",
        page.iter()
            .flat_map(|s| [s.author, s.peer])
            .flatten()
            .collect(),
    )
    .await?;
    let see_body = tx
        .has_field_access(env, "mail.message", "body", erp_sdk::FieldOp::Read)
        .await?;
    let see_names = tx
        .has_field_access(env, "res.partner", "name", erp_sdk::FieldOp::Read)
        .await?;
    let query = query.to_lowercase();
    let items = page
        .into_iter()
        .filter(|item| channels.contains(&item.id))
        .filter_map(|item| {
            let visible = item.latest.is_some_and(|id| messages.contains(&id));
            let name = if see_names && item.peer.is_some_and(|id| partners.contains(&id)) {
                item.peer_name.unwrap_or(item.name)
            } else {
                item.name
            };
            if !query.is_empty() && !name.to_lowercase().contains(&query) {
                return None;
            }
            let sender =
                if visible && see_names && item.author.is_some_and(|id| partners.contains(&id)) {
                    item.sender
                } else {
                    None
                };
            let snippet = if visible && see_body {
                preview(item.body.as_deref().unwrap_or_default())
            } else {
                String::new()
            };
            Some(json!({
                "id":item.id, "name":name, "channel_type":item.kind,
                "latest":item.latest.filter(|_| visible), "seen":item.seen, "owner":item.owner,
                "unread":item.unread, "bookmarks":item.bookmarks, "sender":sender,
                "preview":snippet, "date":item.date.filter(|_|visible),
            }))
        })
        .collect();
    Ok((items, next))
}

pub(super) async fn recent_window(tx: &mut Transaction, env: &Env) -> Result<Vec<i64>> {
    let (items, _) = summaries(tx, env, None, "", "all", None).await?;
    let mut result = Vec::new();
    for item in items {
        for key in ["id", "latest", "seen", "unread", "bookmarks"] {
            result.push(item[key].as_i64().unwrap_or(0));
        }
        result.push(i64::from(item["owner"].as_bool().ok_or_else(bad_row)?));
    }
    Ok(result)
}

// SPDX-License-Identifier: MIT OR Apache-2.0
//! Assistant sessions (D2): an owner-only `ai.session` and its append-only typed
//! event log `ai.session.event`, a weekly leaf-partitioned table. The store is the
//! only writer of events: the published runtime-role grants give it exactly `SELECT` and
//! `INSERT` on the event table, and `SELECT`, `INSERT` and `UPDATE` (bookkeeping) on the session
//! table, so no path can rewrite or delete history. Every write is a validated event of the
//! closed kind set with a versioned payload schema; the sequence number comes from the session
//! row under its lock, which keeps `(session_id, seq)` gapless across nodes.
//!
//! The turn pipeline.1 (slice `AI1.ASSIST-TURN`) calls [`admit_turn`] for
//! admission (lease, company check, caps) and [`append`] for each event; [`fork`] copies a
//! prefix of one session into a new one (5). Listing, search and display of sessions
//! are `AI1.SESSION-VIEW`; retention, deletion and legal hold are `AI1.SESSION-RETENTION`. No
//! provider call ever happens here.

use erp_sdk::{prelude::*, settings::ParameterDecl};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Declared limits. marks every proposed number; amendment L1 makes each one a system
// parameter with the ADR's number as its default, a floor and a hard bound, refused by name
// outside them. Zero never means unlimited.
// ---------------------------------------------------------------------------

/// Events kept per session; past it a turn answers `erp.ai.session_full` (6).
pub const SESSION_MAX_EVENTS: &str = "ai_base.session_max_events";
/// Turns kept per session; past it a turn answers `erp.ai.session_full` (6).
pub const SESSION_MAX_TURNS: &str = "ai_base.session_max_turns";
/// Open sessions one user may hold; past it opening answers `erp.ai.session_limit` (6).
pub const SESSION_MAX_OPEN: &str = "ai_base.session_max_open";
/// Characters of one `user_message` event (6).
pub const MESSAGE_CHARS: &str = "ai_base.session_message_chars";
/// Serialized bytes of one event payload of any other kind (6).
pub const EVENT_BYTES: &str = "ai_base.session_event_bytes";

fn parameters() -> Result<Vec<ParameterDecl>> {
    Ok(vec![
        ParameterDecl::integer(
            SESSION_MAX_EVENTS,
            "Assistant: events per session",
            2000,
            (1, 1_000_000),
        )?,
        ParameterDecl::integer(
            SESSION_MAX_TURNS,
            "Assistant: turns per session",
            200,
            (1, 1_000_000),
        )?,
        ParameterDecl::integer(
            SESSION_MAX_OPEN,
            "Assistant: open sessions per user",
            200,
            (1, 100_000),
        )?,
        ParameterDecl::integer(
            MESSAGE_CHARS,
            "Assistant: user message characters per event",
            16000,
            (1, 1_000_000),
        )?,
        ParameterDecl::integer(
            EVENT_BYTES,
            "Assistant: event payload bytes",
            32768,
            (1, 1_048_576),
        )?,
    ])
}
erp_sdk::system_parameters!(parameters);

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// One user's conversation with the assistant. Owned by exactly one user; the
/// `ir.access` row grants `ai.session` only to the row's owner, so another user and an
/// administrator alike get the concealed access error. The event sequence lives on this row: an
/// append takes `last_seq + 1` under the row lock, which keeps `(session_id, seq)` gapless and
/// unique across nodes without a cross-partition index.
#[model(
    name = "ai.session",
    description = "Assistant Session",
    order = "id desc",
    company_access = unrestricted(reason = "a session is owned by its user and carries the historical company it was opened in; the turn admission checks the caller's current companies, and the session stays readable after the user left the company"),
    constraints = [
        (
            "_state_closed",
            "CHECK (state IN ('open','archived'))",
            "The session state is open or archived."
        ),
        (
            "_counters_positive",
            "CHECK (last_seq >= 0 AND event_count >= 0 AND turn_count >= 0 AND fork_seq >= 0)",
            "Session counters cannot be negative."
        ),
        (
            "_anchor_shape",
            "CHECK ((anchor_model IS NULL AND anchor_res_id IS NULL) OR (anchor_model IS NOT NULL AND anchor_res_id IS NOT NULL))",
            "An anchor names a model and a record together."
        )
    ],
    indexes = [
        ("_owner_state_activity", "(owner_user_id, state, last_event_at DESC)"),
        (
            "_owner_anchor",
            "(owner_user_id, anchor_model, anchor_res_id) WHERE anchor_model IS NOT NULL"
        )
    ]
)]
pub struct AiSession {
    /// The user this conversation belongs to; only they may read it (2).
    #[field(many2one, comodel = "res.users", required = true, ondelete = "cascade")]
    pub owner_user_id: Option<RecId>,
    /// The company the session was opened in; a turn is refused once the user's current
    /// companies no longer contain it (4). Deliberately `unrestricted` rather than
    /// company-confined: the session stays readable after the user left the company, and the
    /// admission check is the store's own.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    pub company_id: Option<RecId>,
    /// Model-written display title; searched through the trigram index (6).
    #[field(char, size = 1024, index = "trigram")]
    pub title: Option<Arc<str>>,
    /// The source-bearing events the title was derived from (3).
    #[field(json)]
    pub title_sources: Option<serde_json::Value>,
    /// `open` or `archived` (1); deletion removes the row (D9.2, another slice).
    #[field(
        selection,
        required = true,
        default = "open",
        selection = [("open", "Open"), ("archived", "Archived")]
    )]
    pub state: Option<Arc<str>>,
    /// Optional anchor: an action external id, or a model and record id (1). An
    /// anchor grants nothing (D1.3).
    #[field(char, size = 255)]
    pub anchor_action: Option<Arc<str>>,
    #[field(char, size = 255)]
    pub anchor_model: Option<Arc<str>>,
    /// The anchored record; the soft reference pair of `anchor_model`.
    #[field(many2one_reference, model_field = "anchor_model")]
    pub anchor_res_id: i64,
    /// For a fork: the parent session. Deleting the parent leaves the fork intact (
    /// D1.5), so the link is cleared instead of cascaded.
    #[field(many2one, comodel = "ai.session", ondelete = "set null", index = true)]
    pub parent_session_id: Option<RecId>,
    /// For a fork: the parent sequence the fork was taken at (5).
    #[field(integer)]
    pub fork_seq: Option<i32>,
    /// The sequence of the latest event; a fork's own sequence continues after its fork point.
    #[field(integer, required = true, default = 0)]
    pub last_seq: i32,
    /// Events appended so far; bounded by `ai_base.session_max_events`.
    #[field(integer, required = true, default = 0)]
    pub event_count: i32,
    /// Turns ended in this session; bounded by `ai_base.session_max_turns`.
    #[field(integer, required = true, default = 0)]
    pub turn_count: i32,
    /// When the session was opened (bookkeeping; `create_date` keeps the server insert time).
    #[field(datetime, required = true)]
    pub created_at: Option<DateTime>,
    /// When the latest event was appended; the open-list activity order.
    #[field(datetime, required = true, index = true)]
    pub last_event_at: Option<DateTime>,
}

/// One append-only event of a session: a closed kind, a versioned payload schema
/// validated on write, and the session's gapless sequence. Weekly partitions on `create_date`
/// : a fork's copies take the fork time as their partition key and keep the
/// original time in `occurred_at`. The runtime role may `SELECT` and `INSERT` and nothing else.
#[model(
    name = "ai.session.event",
    description = "Assistant Session Event",
    addon = "ai_base",
    partition = partition(interval = "week", premake = 2, retain = 58),
    company_access = unrestricted(reason = "events are reachable only through their owner-only session and the store's own reads; no access row grants any group direct model access"),
    constraints = [
        (
            "_kind_closed",
            "CHECK (kind IN ('user_message','assistant_message','tool_call','tool_result','context_loaded','proposal','proposal_outcome','navigation','skill_loaded','memory_proposed','compaction','turn_end'))",
            "The event kind is one of the closed set."
        ),
        (
            "_version_positive",
            "CHECK (payload_version >= 1)",
            "The payload version is positive."
        )
    ],
    unique_indexes = [(
        "_session_seq",
        "(session_id, seq, create_date)",
        "Each assistant session event has one sequence number."
    )]
)]
pub struct AiSessionEvent {
    /// The owning session; events die with it.
    #[field(
        many2one,
        comodel = "ai.session",
        required = true,
        ondelete = "cascade"
    )]
    pub session_id: Option<RecId>,
    /// The event's position in the session, gapless from 1 (5).
    #[field(integer, required = true)]
    pub seq: i32,
    /// The event's business time; a fork's copy keeps the parent's value (5).
    #[field(datetime, required = true)]
    pub occurred_at: Option<DateTime>,
    /// The turn that wrote the event, when one ran.
    #[field(uuid)]
    pub turn_id: Option<Uuid>,
    /// The closed event kind (1).
    #[field(
        selection,
        required = true,
        selection = [
            ("user_message", "User Message"),
            ("assistant_message", "Assistant Message"),
            ("tool_call", "Tool Call"),
            ("tool_result", "Tool Result"),
            ("context_loaded", "Context Loaded"),
            ("proposal", "Proposal"),
            ("proposal_outcome", "Proposal Outcome"),
            ("navigation", "Navigation"),
            ("skill_loaded", "Skill Loaded"),
            ("memory_proposed", "Memory Proposed"),
            ("compaction", "Compaction"),
            ("turn_end", "Turn End")
        ]
    )]
    pub kind: Option<Arc<str>>,
    /// The payload schema version the payload was validated against (1).
    #[field(integer, required = true, default = 1)]
    pub payload_version: i32,
    /// The validated payload; references for source-bearing kinds, never record values (D2.2).
    #[field(json, required = true)]
    pub payload: Option<serde_json::Value>,
    /// The compact references of a source-bearing kind (`tool_result`, `context_loaded`), the
    /// uniform shape the display path reads (4).
    #[field(json)]
    pub source_refs: Option<serde_json::Value>,
    /// The text of a `user_message`, kept for search. The plan's `tsvector` column arrives with
    /// the session search slice; until then the store keeps the plain text.
    #[field(text)]
    pub user_text: Option<Arc<str>>,
}

// ---------------------------------------------------------------------------
// Runtime role grants: the event log is append-only, the session row has no DELETE (
// D2.5, D9.2). The composition root narrows the runtime role's blanket CRUD when it publishes
// grants, calling [`publish_runtime_grants`] beside the other runtime-role grants: the role
// holds exactly `SELECT` and `INSERT` on the event table (UPDATE and DELETE are refused by
// grants), and `SELECT`, `INSERT` and `UPDATE` (bookkeeping) on the session row. Deletion of
// sessions and events goes through the SECURITY DEFINER functions of AI1.SESSION-RETENTION.
// ---------------------------------------------------------------------------

/// Publish the runtime role's rights on the two session tables (5). Idempotent: it
/// revokes everything the role held on them first, then grants exactly the policy. The grant on
/// the event table covers the parent (whose privileges new weekly partitions inherit when the
/// partition upkeep creates them) and every existing partition, so the role's `SELECT` reaches
/// the stored rows.
///
/// # Errors
/// An invalid role name, or the database's errors when the grants cannot be applied.
pub async fn publish_runtime_grants(tx: &mut Transaction, role: &str) -> Result<()> {
    if role.is_empty() || role.len() > 63 || role.chars().any(char::is_control) {
        return Err(Error::validation("invalid assistant session runtime role"));
    }
    let quoted = format!("\"{}\"", role.replace('"', "\"\""));
    tx.batch(&format!(
        "REVOKE ALL ON TABLE ai_session, ai_session_event FROM {quoted}; \
         GRANT SELECT, INSERT ON TABLE ai_session_event TO {quoted}; \
         GRANT SELECT, INSERT, UPDATE ON TABLE ai_session TO {quoted}; \
         DO $$ DECLARE p text; BEGIN \
           FOR p IN SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
                    WHERE i.inhparent = 'ai_session_event'::regclass \
           LOOP \
             EXECUTE 'REVOKE ALL ON TABLE ' || quote_ident(p) || ' FROM {quoted}'; \
             EXECUTE 'GRANT SELECT, INSERT ON TABLE ' || quote_ident(p) || ' TO {quoted}'; \
           END LOOP; \
         END $$;"
    ))
    .await
}

// ---------------------------------------------------------------------------
// The closed event kind set and its versioned payload schemas (1). Every kind has
// exactly one payload version today; a later slice adds a version by extending the match.
// ---------------------------------------------------------------------------

/// The closed set of session event kinds (1). Adding a kind is an ADR change, not a
/// silent extension, so this enum is deliberately exhaustive and not `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    ContextLoaded,
    Proposal,
    ProposalOutcome,
    Navigation,
    SkillLoaded,
    MemoryProposed,
    Compaction,
    TurnEnd,
}

impl EventKind {
    /// Parse the stored kind name; `None` for a name outside the closed set.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "user_message" => Some(Self::UserMessage),
            "assistant_message" => Some(Self::AssistantMessage),
            "tool_call" => Some(Self::ToolCall),
            "tool_result" => Some(Self::ToolResult),
            "context_loaded" => Some(Self::ContextLoaded),
            "proposal" => Some(Self::Proposal),
            "proposal_outcome" => Some(Self::ProposalOutcome),
            "navigation" => Some(Self::Navigation),
            "skill_loaded" => Some(Self::SkillLoaded),
            "memory_proposed" => Some(Self::MemoryProposed),
            "compaction" => Some(Self::Compaction),
            "turn_end" => Some(Self::TurnEnd),
            _ => None,
        }
    }

    /// The stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::AssistantMessage => "assistant_message",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::ContextLoaded => "context_loaded",
            Self::Proposal => "proposal",
            Self::ProposalOutcome => "proposal_outcome",
            Self::Navigation => "navigation",
            Self::SkillLoaded => "skill_loaded",
            Self::MemoryProposed => "memory_proposed",
            Self::Compaction => "compaction",
            Self::TurnEnd => "turn_end",
        }
    }

    /// The only payload version this build validates.
    pub const fn version(self) -> i32 {
        1
    }

    /// The payload rejection naming the kind and the reason.
    fn invalid(self, reason: &str) -> Error {
        Error::validation(format!(
            "erp.ai.event_payload_invalid: '{}': {reason}",
            self.as_str()
        ))
    }

    /// Parse the payload into its versioned schema; a failure names the kind and the offending
    /// field.
    fn schema<T: DeserializeOwned>(self, payload: &serde_json::Value) -> Result<T> {
        serde_json::from_value(payload.clone()).map_err(|error| self.invalid(&error.to_string()))
    }

    /// Validate `payload` against this kind's versioned schema (1). A payload that
    /// does not fit fails the write with an error naming the kind and the offending field.
    ///
    /// # Errors
    /// `Validation` `erp.ai.event_payload_invalid: '<kind>': <reason>`.
    pub fn validate(self, payload: &serde_json::Value) -> Result<()> {
        match self {
            Self::UserMessage => {
                self.schema::<UserMessage>(payload)?;
            }
            Self::AssistantMessage => {
                self.schema::<AssistantMessage>(payload)?;
            }
            Self::ToolCall => {
                self.schema::<ToolCall>(payload)?;
            }
            Self::ToolResult => {
                let result = self.schema::<ToolResult>(payload)?;
                if result.grouped.is_some()
                    && (!result.ids.is_empty() || !result.field_names.is_empty())
                {
                    return Err(self.invalid("a grouped result must not carry ids or field_names"));
                }
            }
            Self::ContextLoaded => {
                self.schema::<ContextLoaded>(payload)?;
            }
            Self::Proposal => {
                self.schema::<Proposal>(payload)?;
            }
            Self::ProposalOutcome => {
                let outcome = self.schema::<ProposalOutcome>(payload)?;
                if !matches!(
                    outcome.outcome.as_str(),
                    "confirmed" | "rejected" | "expired" | "invalidated"
                ) {
                    return Err(self.invalid(&format!(
                        "outcome must be confirmed, rejected, expired or invalidated (got '{}')",
                        outcome.outcome
                    )));
                }
            }
            Self::Navigation => {
                self.schema::<Navigation>(payload)?;
            }
            Self::SkillLoaded => {
                self.schema::<SkillLoaded>(payload)?;
            }
            Self::MemoryProposed => {
                let memory = self.schema::<MemoryProposed>(payload)?;
                if !matches!(memory.scope.as_str(), "user" | "company") {
                    return Err(self.invalid(&format!(
                        "scope must be user or company (got '{}')",
                        memory.scope
                    )));
                }
            }
            Self::Compaction => {
                let compaction = self.schema::<Compaction>(payload)?;
                if compaction.from_seq < 1 || compaction.to_seq < compaction.from_seq {
                    return Err(self.invalid("the covered range must have 1 <= from_seq <= to_seq"));
                }
            }
            Self::TurnEnd => {
                self.schema::<TurnEnd>(payload)?;
            }
        }
        Ok(())
    }

    /// Whether the kind stores record references: the store derives `source_refs` for it.
    const fn source_bearing(self) -> bool {
        matches!(self, Self::ToolResult | Self::ContextLoaded)
    }
}

/// The `user_message` payload, version 1: the user's own text, capped at
/// `ai_base.session_message_chars` characters (6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserMessage {
    pub text: String,
}

/// The `assistant_message` payload, version 1: model-written text (3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantMessage {
    pub text: String,
}

/// The `tool_call` payload, version 1: the tool the model asked for and its arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub tool: String,
    pub call_id: String,
    pub arguments: serde_json::Value,
}

/// The grouped half of a `tool_result`: the domain and group-bys, never the totals (
/// D2.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultGrouped {
    pub model: String,
    pub domain: serde_json::Value,
    pub group_bys: Vec<String>,
    pub aggregates: serde_json::Value,
}

/// The `tool_result` payload, version 1: references, never record values. It holds the model,
/// ids, field names, row count, truncation and the fingerprint over ids and write dates; a
/// grouped result holds the domain and group-bys instead of ids (2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub tool: String,
    pub call_id: String,
    pub model: String,
    #[serde(default)]
    pub ids: Vec<i64>,
    #[serde(default)]
    pub field_names: Vec<String>,
    pub row_count: i64,
    pub truncated: bool,
    pub fingerprint: String,
    pub grouped: Option<ToolResultGrouped>,
}

/// One record set a turn loaded as the user, as references (1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRecord {
    pub model: String,
    pub ids: Vec<i64>,
    pub field_names: Vec<String>,
}

/// The anchor a `context_loaded` event names: an action external id, or a model and record id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Anchor {
    Action { action: String },
    Record { model: String, id: i64 },
}

/// The `context_loaded` payload, version 1: the page context and anchor, as references.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextLoaded {
    pub records: Vec<ContextRecord>,
    pub anchor: Option<Anchor>,
}

/// The `proposal` payload, version 1: a stored proposal this event is about.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub proposal_id: i64,
}

/// The `proposal_outcome` payload, version 1: what happened to the proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalOutcome {
    pub proposal_id: i64,
    pub outcome: String,
}

/// The `navigation` payload, version 1: a view target the client opened.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Navigation {
    pub target: String,
}

/// The `skill_loaded` payload, version 1: the skill by external id and version (3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillLoaded {
    pub skill: String,
    pub version: String,
}

/// The `memory_proposed` payload, version 1: the exact text the user sees on the card (
/// D8.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryProposed {
    pub scope: String,
    pub key: String,
    pub text: String,
}

/// The `compaction` payload, version 1: the covered sequence range and the summary (
/// D4.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compaction {
    pub from_seq: i32,
    pub to_seq: i32,
    pub summary: String,
}

/// The `turn_end` payload, version 1: the turn's outcome and the caps it used (1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnEnd {
    pub outcome: String,
    pub round_trips: i64,
    pub tool_calls: i64,
}

// ---------------------------------------------------------------------------
// Store operations. Every one runs only for an ordinary, unimpersonated user and
// scopes its statements to the owner, so a missing or foreign session answers the same concealed
// access error (2).
// ---------------------------------------------------------------------------

/// What a new session records about where it started (1).
#[derive(Debug)]
pub struct NewSession {
    /// The caller's current company; must be among the caller's company memberships.
    pub company: i64,
    pub title: Option<Arc<str>>,
    pub anchor_action: Option<Arc<str>>,
    pub anchor_model: Option<Arc<str>>,
    pub anchor_res_id: i64,
}

impl NewSession {
    /// A session without a title or an anchor, in `company`.
    pub const fn new(company: i64) -> Self {
        Self {
            company,
            title: None,
            anchor_action: None,
            anchor_model: None,
            anchor_res_id: 0,
        }
    }
}

/// One event to append.
#[derive(Debug)]
pub struct NewEvent<'a> {
    /// The stored kind name; must be in the closed set.
    pub kind: &'a str,
    /// The payload schema version; must be the kind's current one.
    pub version: i32,
    /// The running turn, when one writes the event.
    pub turn_id: Option<Uuid>,
    /// The payload; validated against the kind's schema before anything is written.
    pub payload: serde_json::Value,
}

fn require_user(tx: &Transaction) -> Result<i64> {
    if !crate::ordinary_user(&tx.env()) {
        return Err(Error::access("An interactive session is required."));
    }
    Ok(tx.env().user_id().get())
}

/// The caller's current company memberships, read from the membership table the principal
/// resolver builds the default company set from. The admission checks the session's company
/// against this set (4); it deliberately does not read the request context, so the
/// check works the same on every node and in tests.
async fn company_memberships(tx: &mut Transaction, uid: i64) -> Result<Vec<i64>> {
    let rows = tx
        .query(
            "SELECT cid::int8 FROM res_company_users_rel WHERE user_id = $1 ORDER BY 1",
            &[&uid],
        )
        .await?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

/// The concealed access error for a session the caller does not own or that does not exist: the
/// same wording either way, so existence is never revealed (2).
fn concealed() -> Error {
    Error::access(
        "Your access rights do not let you read 'Assistant Session' (ai.session) records.",
    )
}

/// Open a session for the current user in their current company (1), refusing an
/// invalid anchor or the open-session cap (6).
///
/// # Errors
/// `Access` for a non-interactive caller or no current company; `Validation` for an anchor
/// whose model is unknown to the registry or whose parts contradict; `User`
/// `erp.ai.session_limit` past the open-session cap.
pub async fn open_session(tx: &mut Transaction, new: &NewSession) -> Result<i64> {
    let uid = require_user(tx)?;
    let memberships = company_memberships(tx, uid).await?;
    if !memberships.contains(&new.company) {
        return Err(Error::access("Select a company."));
    }
    let has_anchor =
        new.anchor_action.is_some() || new.anchor_model.is_some() || new.anchor_res_id != 0;
    if has_anchor {
        let action = new.anchor_action.is_some();
        let record = new.anchor_model.is_some() && new.anchor_res_id != 0;
        if action == record {
            return Err(Error::validation(
                "erp.ai.anchor_invalid: an anchor is either an action external id or a model and record id.",
            ));
        }
        if new.anchor_model.is_some() != (new.anchor_res_id != 0) {
            return Err(Error::validation(
                "erp.ai.anchor_invalid: an anchor names a model and a record together.",
            ));
        }
        if let Some(model) = &new.anchor_model
            && !model.is_empty()
            && !tx
                .env()
                .registry()
                .models()
                .iter()
                .any(|m| m.name == model.as_ref())
        {
            return Err(Error::validation(format!(
                "erp.ai.anchor_invalid: '{model}' is not a known model."
            )));
        }
    }
    let limit = i64::try_from(crate::catalog::limit(tx, SESSION_MAX_OPEN).await?)
        .map_err(|_| Error::validation("Invalid assistant open-session limit."))?;
    let open = tx
        .query(
            "SELECT count(*)::int8 FROM ai_session WHERE owner_user_id = $1 AND state = 'open'",
            &[&uid],
        )
        .await?[0]
        .get::<_, i64>(0);
    if open >= limit {
        return Err(Error::user(
            "erp.ai.session_limit: the open-session limit is reached; archive a session to open a new one.",
        ));
    }
    let inserted = tx
        .query(
            "INSERT INTO ai_session (create_date, write_date, owner_user_id, company_id, title, \
             state, anchor_action, anchor_model, anchor_res_id, last_seq, event_count, turn_count, \
             created_at, last_event_at) \
             VALUES ((now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC'), $1, $2, $3, 'open', \
             $4, $5, $6, 0, 0, 0, (now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC')) \
             RETURNING id::int8",
            &[
                &uid,
                &new.company,
                &new.title.as_deref(),
                &new.anchor_action.as_deref(),
                &new.anchor_model.as_deref(),
                &(new.anchor_res_id != 0).then_some(new.anchor_res_id),
            ],
        )
        .await?;
    Ok(inserted[0].get(0))
}

/// Admit one turn into `session`: the session must belong to the caller, its company must
/// still be among the caller's current companies, it must be under the event and turn caps, and
/// no other turn may hold its lease (1, D1.4, D1.6, D11). The lease is an advisory
/// lock bound to this transaction, keyed by the session: it releases when the
/// turn's transaction ends, so a node that dies frees the session without a timer.
///
/// # Errors
/// `Access` (concealed) for a missing or foreign session or a non-interactive caller; `User`
/// `erp.ai.session_company_changed`, `erp.ai.session_full` or `erp.ai.busy` for the three
/// refusals.
pub async fn admit_turn(tx: &mut Transaction, session: i64) -> Result<()> {
    admit_turn_with_capacity(tx, session, 1).await
}

pub(crate) async fn admit_turn_with_capacity(
    tx: &mut Transaction,
    session: i64,
    required_events: i64,
) -> Result<()> {
    let uid = require_user(tx)?;
    let locked = tx
        .query(
            "SELECT pg_try_advisory_xact_lock(hashtextextended('ai.session.turn.' || $1::bigint::text, 0))",
            &[&session],
        )
        .await?[0]
        .get::<_, bool>(0);
    if !locked {
        return Err(Error::user(
            "erp.ai.busy: another assistant turn is in progress in this session.",
        ));
    }
    let row = tx
        .query(
            "SELECT company_id::int8, event_count, turn_count FROM ai_session \
             WHERE id = $1 AND owner_user_id = $2",
            &[&session, &uid],
        )
        .await?;
    let Some(row) = row.first() else {
        return Err(concealed());
    };
    let company: i64 = row.get(0);
    let memberships = company_memberships(tx, uid).await?;
    if !memberships.contains(&company) {
        return Err(Error::user(
            "erp.ai.session_company_changed: this session belongs to a company that is no longer \
             among your current companies; open a new session.",
        ));
    }
    let max_events = i64::try_from(crate::catalog::limit(tx, SESSION_MAX_EVENTS).await?)
        .map_err(|_| Error::validation("Invalid assistant event limit."))?;
    let max_turns = i64::try_from(crate::catalog::limit(tx, SESSION_MAX_TURNS).await?)
        .map_err(|_| Error::validation("Invalid assistant turn limit."))?;
    if i64::from(row.get::<_, i32>(1)) + required_events > max_events
        || i64::from(row.get::<_, i32>(2)) >= max_turns
    {
        return Err(Error::user(
            "erp.ai.session_full: the session reached its event or turn limit; fork from the latest summary.",
        ));
    }
    Ok(())
}

/// Check that the session's company is still among the caller's current companies, for a call
/// that continues a session without running a new turn (4).
///
/// # Errors
/// `Access` (concealed) for a missing or foreign session or a non-interactive caller; `User`
/// `erp.ai.session_company_changed` when the company left the caller's current set.
pub async fn ensure_session_company(tx: &mut Transaction, session: i64) -> Result<()> {
    let uid = require_user(tx)?;
    let row = tx
        .query(
            "SELECT company_id::int8 FROM ai_session WHERE id = $1 AND owner_user_id = $2",
            &[&session, &uid],
        )
        .await?;
    let Some(row) = row.first() else {
        return Err(concealed());
    };
    let company: i64 = row.get(0);
    let memberships = company_memberships(tx, uid).await?;
    if !memberships.contains(&company) {
        return Err(Error::user(
            "erp.ai.session_company_changed: this session belongs to a company that is no longer \
             among your current companies; open a new session.",
        ));
    }
    Ok(())
}

/// Append one validated event to the caller's own session and return its sequence number. The
/// sequence is taken from the session row under its lock, so concurrent appends serialize and
/// the sequence stays gapless (5, D11). Nothing is appended when a check refuses.
///
/// # Errors
/// `Access` (concealed) for a missing or foreign session or a non-interactive caller;
/// `Validation` `erp.ai.event_kind_invalid` naming an unknown kind, `erp.ai.event_version_invalid`
/// naming the kind and version, or `erp.ai.event_payload_invalid` naming the kind and the
/// offending field; `User` `erp.ai.event_too_large` naming the kind and bound, or
/// `erp.ai.session_full` at the event cap.
pub async fn append(tx: &mut Transaction, session: i64, event: &NewEvent<'_>) -> Result<i32> {
    let uid = require_user(tx)?;
    let Some(kind) = EventKind::parse(event.kind) else {
        return Err(Error::validation(format!(
            "erp.ai.event_kind_invalid: '{kind}' is not a known assistant event kind.",
            kind = event.kind
        )));
    };
    if event.version != kind.version() {
        return Err(Error::validation(format!(
            "erp.ai.event_version_invalid: '{}' has no payload version {}.",
            event.kind, event.version
        )));
    }
    kind.validate(&event.payload)?;
    let user_text = if kind == EventKind::UserMessage {
        let message: UserMessage = kind.schema(&event.payload)?;
        let cap = crate::catalog::limit(tx, MESSAGE_CHARS).await?;
        if message.text.chars().count() > cap {
            return Err(Error::user(format!(
                "erp.ai.event_too_large: 'user_message' exceeds {cap} characters; nothing was appended."
            )));
        }
        Some(message.text)
    } else {
        let bytes = serde_json::to_vec(&event.payload)
            .map_err(|error| Error::validation(format!("erp.ai.event_payload_invalid: {error}")))?
            .len();
        let cap = crate::catalog::limit(tx, EVENT_BYTES).await?;
        if bytes > cap {
            return Err(Error::user(format!(
                "erp.ai.event_too_large: '{}' exceeds {cap} bytes; nothing was appended.",
                event.kind
            )));
        }
        None
    };
    insert_event(tx, session, uid, kind, event, user_text.as_deref()).await
}

/// The shared write path of [`append`]: take the sequence under the session row lock, then
/// insert the row. Both statements run in the caller's transaction.
async fn insert_event(
    tx: &mut Transaction,
    session: i64,
    uid: i64,
    kind: EventKind,
    event: &NewEvent<'_>,
    user_text: Option<&str>,
) -> Result<i32> {
    let max_events = i32::try_from(crate::catalog::limit(tx, SESSION_MAX_EVENTS).await?)
        .map_err(|_| Error::validation("Invalid assistant event limit."))?;
    let seq = tx
        .query(
            "UPDATE ai_session SET last_seq = last_seq + 1, event_count = event_count + 1, \
             last_event_at = (now() AT TIME ZONE 'UTC'), \
             turn_count = turn_count + (($4::text = 'turn_end')::int) \
             WHERE id = $1 AND owner_user_id = $2 AND event_count < $3 \
             RETURNING last_seq",
            &[&session, &uid, &max_events, &event.kind],
        )
        .await?;
    let Some(seq) = seq.first() else {
        let exists = tx
            .query(
                "SELECT 1 FROM ai_session WHERE id = $1 AND owner_user_id = $2",
                &[&session, &uid],
            )
            .await?;
        if exists.is_empty() {
            return Err(concealed());
        }
        return Err(Error::user(
            "erp.ai.session_full: the session reached its event or turn limit; fork from the latest summary.",
        ));
    };
    let seq: i32 = seq.get(0);
    let refs = if kind.source_bearing() {
        Some(
            serde_json::to_string(&source_refs(kind, &event.payload)?).map_err(|error| {
                Error::internal(format!("Could not serialize event references: {error}"))
            })?,
        )
    } else {
        None
    };
    let payload = serde_json::to_string(&event.payload)
        .map_err(|error| Error::internal(format!("Could not serialize event payload: {error}")))?;
    tx.execute(
        "INSERT INTO ai_session_event (create_date, write_date, session_id, seq, occurred_at, \
         turn_id, kind, payload_version, payload, source_refs, user_text) \
         VALUES ((now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC'), $1, $2, \
         (now() AT TIME ZONE 'UTC'), $3::text::uuid, $4, $5, $6::text::jsonb, $7::text::jsonb, $8)",
        &[
            &session,
            &seq,
            &event.turn_id.map(|token| token.to_string()),
            &event.kind,
            &event.version,
            &payload,
            &refs,
            &user_text,
        ],
    )
    .await?;
    Ok(seq)
}

/// The compact references of a source-bearing event (4): ids and field names, or the
/// domain and group-bys of a grouped result; the payload itself was already validated.
fn source_refs(kind: EventKind, payload: &serde_json::Value) -> Result<serde_json::Value> {
    match kind {
        EventKind::ToolResult => {
            let result: ToolResult = serde_json::from_value(payload.clone()).map_err(|error| {
                Error::validation(format!(
                    "erp.ai.event_payload_invalid: 'tool_result': {error}"
                ))
            })?;
            Ok(match result.grouped {
                Some(grouped) => serde_json::json!({
                    "model": grouped.model,
                    "domain": grouped.domain,
                    "group_bys": grouped.group_bys,
                }),
                None => serde_json::json!({
                    "model": result.model,
                    "ids": result.ids,
                    "field_names": result.field_names,
                }),
            })
        }
        EventKind::ContextLoaded => {
            let loaded: ContextLoaded =
                serde_json::from_value(payload.clone()).map_err(|error| {
                    Error::validation(format!(
                        "erp.ai.event_payload_invalid: 'context_loaded': {error}"
                    ))
                })?;
            Ok(serde_json::json!({
                "records": loaded.records,
                "anchor": loaded.anchor,
            }))
        }
        other => Err(Error::programming(format!(
            "event kind '{}' is not source-bearing",
            other.as_str()
        ))),
    }
}

/// Fork `session` at `at_seq` into a new session owned by the same user (5): the
/// fork holds copies of every event up to the fork point with its own sequence continuing after
/// it. The copies take the fork time as their partition key and keep the original `occurred_at`.
///
/// # Errors
/// `Access` (concealed) for a missing or foreign session or a non-interactive caller;
/// `Validation` for a fork point outside `1..=last_seq`.
pub async fn fork(tx: &mut Transaction, session: i64, at_seq: i32) -> Result<i64> {
    let uid = require_user(tx)?;
    let parent = tx
        .query(
            "SELECT last_seq FROM ai_session WHERE id = $1 AND owner_user_id = $2",
            &[&session, &uid],
        )
        .await?;
    let Some(parent) = parent.first() else {
        return Err(concealed());
    };
    let last_seq: i32 = parent.get(0);
    if !(1..=last_seq).contains(&at_seq) {
        return Err(Error::validation(format!(
            "Invalid fork point {at_seq}: the session has {last_seq} events."
        )));
    }
    let created = tx
        .query(
            "INSERT INTO ai_session (create_date, write_date, owner_user_id, company_id, state, \
             parent_session_id, fork_seq, last_seq, event_count, turn_count, created_at, \
             last_event_at) \
             SELECT (now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC'), s.owner_user_id, \
             s.company_id, 'open', s.id, $2, $2, $2, \
             (SELECT count(*)::int4 FROM ai_session_event e WHERE e.session_id = s.id \
             AND e.seq <= $2 AND e.kind = 'turn_end'), \
             (now() AT TIME ZONE 'UTC'), \
             COALESCE((SELECT max(e.occurred_at) FROM ai_session_event e WHERE e.session_id = s.id \
             AND e.seq <= $2), (now() AT TIME ZONE 'UTC')) \
             FROM ai_session s WHERE s.id = $1 AND s.owner_user_id = $3 \
             RETURNING id::int8",
            &[&session, &at_seq, &uid],
        )
        .await?;
    let created: i64 = created[0].get(0);
    let copied = tx
        .execute(
            "INSERT INTO ai_session_event (create_date, write_date, session_id, seq, occurred_at, \
             turn_id, kind, payload_version, payload, source_refs, user_text) \
             SELECT (now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC'), $2, e.seq, \
             e.occurred_at, e.turn_id, e.kind, e.payload_version, e.payload, e.source_refs, \
             e.user_text \
             FROM ai_session_event e WHERE e.session_id = $1 AND e.seq <= $3",
            &[&session, &created, &at_seq],
        )
        .await?;
    if copied != u64::from(at_seq as u32) {
        return Err(Error::internal(format!(
            "Fork of session {session} copied {copied} of {at_seq} events."
        )));
    }
    Ok(created)
}

/// SHA-256 over canonical JSON of the model, the ids sorted with each id's `write_date`, and the
/// sorted field names: the fingerprint of a `tool_result` over records (2). It is
/// over ids and write dates, never over field values, so it cannot confirm a guessed value; it
/// changes exactly when a covered record's `write_date` changes. The object is built in a
/// `BTreeMap`, whose keys serialize in sorted order — the RFC 8785 canonical order for this
/// shape of strings, integers, arrays and objects.
///
/// # Errors
/// `Internal` when the canonical input cannot be serialized (a defect, not input).
pub fn fingerprint(
    model: &str,
    ids: &[i64],
    write_dates: &[DateTime],
    field_names: &[&str],
) -> Result<String> {
    let mut pairs: Vec<(i64, DateTime)> = ids
        .iter()
        .copied()
        .zip(write_dates.iter().copied())
        .collect();
    pairs.sort_unstable_by_key(|(id, _)| *id);
    let mut fields: Vec<&str> = field_names.to_vec();
    fields.sort_unstable();
    let mut map = std::collections::BTreeMap::new();
    map.insert("model", serde_json::Value::String(model.to_owned()));
    map.insert(
        "ids",
        serde_json::Value::Array(
            pairs
                .iter()
                .map(|(id, _)| serde_json::Value::from(*id))
                .collect(),
        ),
    );
    map.insert(
        "write_dates",
        serde_json::Value::Array(
            pairs
                .iter()
                .map(|(_, date)| serde_json::Value::String(date.to_string()))
                .collect(),
        ),
    );
    map.insert(
        "fields",
        serde_json::Value::Array(
            fields
                .iter()
                .map(|field| serde_json::Value::String((*field).to_owned()))
                .collect(),
        ),
    );
    let bytes = serde_json::to_vec(&map).map_err(|error| {
        Error::internal(format!(
            "Could not serialize the result fingerprint: {error}"
        ))
    })?;
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(hex)
}

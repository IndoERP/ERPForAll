use erp_fields::{Error, Result};
use erp_orm::{Transaction, UserId};

pub const DEFAULT_LANG: &str = "en_US";

/// # Errors
///
/// The statement's.
pub async fn installed_languages(tx: &mut Transaction) -> Result<Vec<String>> {
    if tx.env().registry().model("res.lang").is_none() {
        return Ok(vec![DEFAULT_LANG.to_owned()]);
    }
    let rows = tx
        .query(
            "SELECT code FROM res_lang WHERE active ORDER BY name, code",
            &[],
        )
        .await?;
    rows.iter()
        .map(|r| {
            r.try_get::<_, String>(0)
                .map_err(|e| Error::db(format!("res_lang.code: {e}")).with_source(e))
        })
        .collect()
}

pub fn context_lang(user_lang: Option<&str>, installed: &[String]) -> String {
    match user_lang {
        Some(lang) if installed.iter().any(|l| l == lang) => lang.to_owned(),
        _ if installed.iter().any(|l| l == DEFAULT_LANG) => DEFAULT_LANG.to_owned(),
        _ => installed
            .first()
            .map_or_else(|| DEFAULT_LANG.to_owned(), Clone::clone),
    }
}

/// A user's context: `lang` (an installed language), `tz` (the user's, `None` when unset) and
/// `uid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserContext {
    uid: UserId,
    lang: String,
    tz: Option<String>,
}

impl UserContext {
    pub(crate) fn new(uid: UserId, lang: String, tz: Option<String>) -> Self {
        Self { uid, lang, tz }
    }

    /// The user.
    pub fn uid(&self) -> UserId {
        self.uid
    }

    /// The language code.
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// The time zone name.
    pub fn tz(&self) -> Option<&str> {
        self.tz.as_deref()
    }
}

/// # Errors
///
/// The statements'.
pub async fn user_context(tx: &mut Transaction, uid: UserId) -> Result<Option<UserContext>> {
    let rows = tx
        .query(
            "SELECT lang, tz FROM res_users WHERE id = $1",
            &[&uid.get()],
        )
        .await?;
    let (lang, tz): (Option<String>, Option<String>) = match rows.as_slice() {
        [row] => (
            row.try_get(0)
                .map_err(|e| Error::db(format!("res_users.lang: {e}")).with_source(e))?,
            row.try_get(1)
                .map_err(|e| Error::db(format!("res_users.tz: {e}")).with_source(e))?,
        ),
        [] => return Ok(None),
        more => {
            return Err(Error::programming(format!(
                "{} res_users rows with id {}",
                more.len(),
                uid.get()
            )));
        }
    };
    let installed = installed_languages(tx).await?;
    Ok(Some(UserContext::new(
        uid,
        context_lang(lang.as_deref(), &installed),
        tz,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_chain() {
        let both = vec!["en_US".to_owned(), "fr_FR".to_owned()];
        assert_eq!(context_lang(Some("fr_FR"), &both), "fr_FR");
        assert_eq!(context_lang(Some("de_DE"), &both), "en_US");
        assert_eq!(context_lang(None, &both), "en_US");
        let fr = vec!["fr_FR".to_owned()];
        assert_eq!(context_lang(Some("de_DE"), &fr), "fr_FR");
        assert_eq!(context_lang(None, &[]), "en_US");
    }
}

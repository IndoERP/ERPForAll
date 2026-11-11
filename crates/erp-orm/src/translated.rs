//! Translated field projection, mutation, and delta generation.

use erp_fields::{Cell, Error, FieldType, Result};
use erp_registry::FieldSpec;
use std::sync::Arc;

use crate::lang::{ActiveLanguages, validate_lang};

/// Project the requested language from a cached raw JSONB cell.
///
/// Falls back to `"en_US"` if the requested language is missing. A cell that is neither
/// NULL, a string nor a JSON object is a programming error naming the field, never a silent NULL.
///
/// # Errors
///
/// `Programming` for a cell shape a translated slot never holds.
pub fn project(model: &str, field: &str, cell: &Cell, lang: Option<&str>) -> Result<Cell> {
    if matches!(cell, Cell::Json(j) if j.is_object()) {
        erp_fields::FieldTranslations::validate_cell(cell)
            .map_err(|err| Error::programming(format!("{model}.{field}: {}", err.message())))?;
    }
    match cell {
        Cell::Null => Ok(Cell::Null),
        Cell::Json(v) => match v.as_ref() {
            serde_json::Value::Object(map) => {
                let target_lang = lang.filter(|l| !l.is_empty()).unwrap_or("en_US");
                if let Some(s) = map.get(target_lang).and_then(json_str) {
                    Ok(Cell::Str(Arc::from(s)))
                } else if target_lang != "en_US"
                    && let Some(s) = map.get("en_US").and_then(json_str)
                {
                    Ok(Cell::Str(Arc::from(s)))
                } else {
                    Ok(Cell::Null)
                }
            }
            other => Err(Error::programming(format!(
                "{model}.{field}: translated cell holds a JSON {} instead of a language map",
                json_kind(other)
            ))),
        },
        Cell::Str(s) => Ok(Cell::Str(Arc::clone(s))),
        other => Err(Error::programming(format!(
            "{model}.{field}: translated cell has unexpected shape {other:?}"
        ))),
    }
}

fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn json_str(v: &serde_json::Value) -> Option<&str> {
    match v {
        serde_json::Value::String(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Apply a write or create cell to a translated field.
///
/// Handles `Cell::Null` (clearing all languages), `Cell::Str` (per-language write with English
/// seeding and inactive English synchronization), and `Cell::Json` (translation dictionary).
pub fn apply_write(
    field: &FieldSpec,
    old_cell: Option<&Cell>,
    new_cell: &Cell,
    lang: Option<&str>,
    active_langs: &ActiveLanguages,
) -> Result<Cell> {
    let cell = apply_write_unchecked(field, old_cell, new_cell, lang, active_langs)?;
    if field.required && cell.is_null() {
        return Err(Error::validation(format!(
            "Missing required field '{}': it cannot be cleared",
            field.name
        )));
    }
    if field.required
        && let Cell::Json(v) = &cell
    {
        // A required translated field needs a non-empty base (`en_US`) value; the NOT NULL
        // jsonb column alone would accept `{"en_US": ""}` (lead decision 4, 2026-10-27).
        let base_ok = v
            .get("en_US")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if !base_ok {
            return Err(Error::validation(format!(
                "Missing required field '{}': the base (en_US) value must be non-empty",
                field.name
            )));
        }
    }
    Ok(cell)
}

pub(crate) fn apply_write_unchecked(
    field: &FieldSpec,
    old_cell: Option<&Cell>,
    new_cell: &Cell,
    lang: Option<&str>,
    active_langs: &ActiveLanguages,
) -> Result<Cell> {
    if new_cell.is_null() || matches!(new_cell, Cell::Bool(false)) {
        return Ok(Cell::Null);
    }
    match new_cell {
        Cell::Json(v) => {
            let serde_json::Value::Object(dict) = v.as_ref() else {
                return Err(Error::validation(if matches!(field.ty, FieldType::Html) {
                    format!("{}: expected an HTML translation map", field.name)
                } else {
                    format!("Expected JSON object for translated field, got {v}")
                }));
            };
            if dict
                .iter()
                .any(|(k, _)| !active_langs.codes().contains(k) && k != "en_US")
            {
                return Err(Error::validation(if matches!(field.ty, FieldType::Html) {
                    format!(
                        "{}: HTML translation contains an inactive language",
                        field.name
                    )
                } else {
                    format!("Some of the languages are not active: {v}")
                }));
            }
            if dict
                .iter()
                .any(|(_, val)| val.is_null() || matches!(val, serde_json::Value::Bool(false)))
            {
                return Err(Error::validation(if matches!(field.ty, FieldType::Html) {
                    format!("{}: HTML translations require text", field.name)
                } else {
                    format!("Some of the translations are False: {v}")
                }));
            }
            if let FieldType::Char { size: Some(limit) } = field.ty {
                for (k, val) in dict {
                    if let serde_json::Value::String(s) = val {
                        let len = s.chars().count();
                        if len > limit as usize {
                            return Err(Error::validation(format!(
                                "Value too long for {}.{} (at most {limit} characters, got {len}): '{s}'",
                                field.name, k
                            )));
                        }
                    }
                }
            }
            let mut map = match old_cell {
                Some(Cell::Json(old_v)) => match old_v.as_ref() {
                    serde_json::Value::Object(m) => m.clone(),
                    _ => serde_json::Map::new(),
                },
                _ => serde_json::Map::new(),
            };
            for (k, val) in dict {
                map.insert(k.clone(), val.clone());
            }
            if !active_langs.english_active()
                && !dict.contains_key("en_US")
                && let Some((_, first_val)) = dict.iter().next()
            {
                map.insert("en_US".to_string(), first_val.clone());
            }
            Ok(Cell::Json(Arc::new(serde_json::Value::Object(map))))
        }
        Cell::Str(s) => {
            let lang_code = lang.filter(|l| !l.is_empty()).unwrap_or("en_US");
            validate_lang(Some(lang_code), active_langs.codes())?;
            if let FieldType::Char { size: Some(limit) } = field.ty {
                let len = s.chars().count();
                if len > limit as usize {
                    return Err(Error::validation(format!(
                        "Value too long for {} (at most {limit} characters, got {len}): '{s}'",
                        field.name
                    )));
                }
            }
            let mut map = match old_cell {
                Some(Cell::Json(old_v)) => match old_v.as_ref() {
                    serde_json::Value::Object(m) => m.clone(),
                    _ => serde_json::Map::new(),
                },
                _ => serde_json::Map::new(),
            };
            map.insert(
                lang_code.to_string(),
                serde_json::Value::String(s.to_string()),
            );
            // Seeding Rule: if en_US absent or previously NULL, seed en_US -> s
            if !map.contains_key("en_US") {
                map.insert(
                    "en_US".to_string(),
                    serde_json::Value::String(s.to_string()),
                );
            }
            // Inactive English Sync: if en_US inactive, sync en_US -> s
            if !active_langs.english_active() {
                map.insert(
                    "en_US".to_string(),
                    serde_json::Value::String(s.to_string()),
                );
            }
            Ok(Cell::Json(Arc::new(serde_json::Value::Object(map))))
        }
        _ => Err(Error::programming(if matches!(field.ty, FieldType::Html) {
            format!("{}: expected HTML text or a translation map", field.name)
        } else {
            format!(
                "invalid value {new_cell:?} for translated field {}",
                field.name
            )
        })),
    }
}

/// Compute the delta between a baseline raw JSON cell and the current cache cell.
///
/// - If `current_cell` is null, returns `Cell::Null`.
/// - If `base_cell` is missing or null, returns the current object.
/// - Otherwise, returns an object containing only added/modified keys, and deleted keys set to JSON `null`.
///
/// # Errors
///
/// `Programming` when the current cell is not a language map (or NULL); `what` names the field.
pub fn delta(what: &str, base_cell: Option<&Cell>, current_cell: &Cell) -> Result<Cell> {
    if current_cell.is_null() {
        return Ok(Cell::Null);
    }
    let Cell::Json(cur_v) = current_cell else {
        return Err(Error::programming(format!(
            "{what}: translated flush found a non-JSON cell {current_cell:?}"
        )));
    };
    let serde_json::Value::Object(cur_map) = cur_v.as_ref() else {
        return Err(Error::programming(format!(
            "{what}: translated flush found a JSON {} instead of a language map",
            json_kind(cur_v)
        )));
    };
    let base_map = match base_cell {
        Some(Cell::Json(bv)) => match bv.as_ref() {
            serde_json::Value::Object(bm) => Some(bm),
            _ => None,
        },
        _ => None,
    };
    let mut delta_map = serde_json::Map::new();
    match base_map {
        None => {
            for (k, v) in cur_map {
                delta_map.insert(k.clone(), v.clone());
            }
        }
        Some(bm) => {
            for (k, v) in cur_map {
                if bm.get(k) != Some(v) {
                    delta_map.insert(k.clone(), v.clone());
                }
            }
            for k in bm.keys() {
                if !cur_map.contains_key(k) {
                    delta_map.insert(k.clone(), serde_json::Value::Null);
                }
            }
        }
    }
    Ok(Cell::Json(Arc::new(serde_json::Value::Object(delta_map))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

    fn test_field(ty: FieldType) -> FieldSpec {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "test_addon",
            depends: &[],
            exact_only: false,
        };
        let field = FieldDecl::char("name").translate(true);
        let decl = Box::leak(Box::new(
            ModelDecl::new("test_addon", "test.model").fields(Vec::leak(vec![field])),
        ));
        let reg = RegistryBuilder::new([&ADDON])
            .with_decls([&*decl])
            .build()
            .unwrap();
        let mut fs = reg
            .model("test.model")
            .unwrap()
            .field("name")
            .unwrap()
            .clone();
        fs.ty = ty;
        fs
    }

    #[test]
    fn test_project_fallback() {
        let mut map = serde_json::Map::new();
        map.insert(
            "en_US".to_string(),
            serde_json::Value::String("Customers".into()),
        );
        map.insert(
            "fr_FR".to_string(),
            serde_json::Value::String("Clients".into()),
        );
        let cell = Cell::Json(Arc::new(serde_json::Value::Object(map)));

        assert_eq!(
            project("m", "f", &cell, None).unwrap(),
            Cell::Str("Customers".into())
        );
        assert_eq!(
            project("m", "f", &cell, Some("")).unwrap(),
            Cell::Str("Customers".into())
        );
        assert_eq!(
            project("m", "f", &cell, Some("en_US")).unwrap(),
            Cell::Str("Customers".into())
        );
        assert_eq!(
            project("m", "f", &cell, Some("fr_FR")).unwrap(),
            Cell::Str("Clients".into())
        );
        assert_eq!(
            project("m", "f", &cell, Some("es_ES")).unwrap(),
            Cell::Str("Customers".into())
        );

        assert_eq!(
            project("m", "f", &Cell::Null, Some("fr_FR")).unwrap(),
            Cell::Null
        );
    }

    #[test]
    fn test_apply_write_scalar() {
        let field = test_field(FieldType::Char { size: None });
        let active =
            ActiveLanguages::new(["en_US", "fr_FR"].map(String::from).into_iter().collect());

        // Create in fr_FR seeds en_US
        let res = apply_write(
            &field,
            None,
            &Cell::Str("French".into()),
            Some("fr_FR"),
            &active,
        )
        .unwrap();
        let Cell::Json(v) = res else { panic!() };
        assert_eq!(v["fr_FR"], "French");
        assert_eq!(v["en_US"], "French");

        // Write English updates en_US while preserving French
        let res2 = apply_write(
            &field,
            Some(&Cell::Json(v)),
            &Cell::Str("English".into()),
            Some("en_US"),
            &active,
        )
        .unwrap();
        let Cell::Json(v2) = res2 else { panic!() };
        assert_eq!(v2["fr_FR"], "French");
        assert_eq!(v2["en_US"], "English");

        // Write False clears
        let res3 = apply_write(
            &field,
            Some(&Cell::Json(v2)),
            &Cell::Bool(false),
            Some("en_US"),
            &active,
        )
        .unwrap();
        assert_eq!(res3, Cell::Null);
    }

    #[test]
    fn test_apply_write_dict() {
        let field = test_field(FieldType::Char { size: None });
        let active =
            ActiveLanguages::new(["en_US", "fr_FR"].map(String::from).into_iter().collect());

        let mut dict = serde_json::Map::new();
        dict.insert("en_US".to_string(), serde_json::Value::String("A".into()));
        dict.insert("fr_FR".to_string(), serde_json::Value::String("B".into()));

        let res = apply_write(
            &field,
            None,
            &Cell::Json(Arc::new(serde_json::Value::Object(dict))),
            None,
            &active,
        )
        .unwrap();
        let Cell::Json(v) = res else { panic!() };
        assert_eq!(v["en_US"], "A");
        assert_eq!(v["fr_FR"], "B");

        // Inactive language error
        let mut bad_dict = serde_json::Map::new();
        bad_dict.insert("nl_NL".to_string(), serde_json::Value::String("C".into()));
        let err = apply_write(
            &field,
            None,
            &Cell::Json(Arc::new(serde_json::Value::Object(bad_dict))),
            None,
            &active,
        )
        .unwrap_err();
        assert!(
            err.message()
                .contains("Some of the languages are not active")
        );

        // False value error
        let mut false_dict = serde_json::Map::new();
        false_dict.insert("fr_FR".to_string(), serde_json::Value::Bool(false));
        let err2 = apply_write(
            &field,
            None,
            &Cell::Json(Arc::new(serde_json::Value::Object(false_dict))),
            None,
            &active,
        )
        .unwrap_err();
        assert!(
            err2.message()
                .contains("Some of the translations are False")
        );
    }

    #[test]
    fn test_delta() {
        let mut base_map = serde_json::Map::new();
        base_map.insert(
            "en_US".to_string(),
            serde_json::Value::String("Hello".into()),
        );
        base_map.insert(
            "de_DE".to_string(),
            serde_json::Value::String("Hallo".into()),
        );
        let base = Cell::Json(Arc::new(serde_json::Value::Object(base_map)));

        let mut cur_map = serde_json::Map::new();
        cur_map.insert(
            "en_US".to_string(),
            serde_json::Value::String("Hello".into()),
        );
        cur_map.insert(
            "fr_FR".to_string(),
            serde_json::Value::String("Bonjour".into()),
        );
        let cur = Cell::Json(Arc::new(serde_json::Value::Object(cur_map)));

        let d = delta("m.f", Some(&base), &cur).unwrap();
        let Cell::Json(v) = d else { panic!() };
        assert_eq!(v.as_object().unwrap().len(), 2);
        assert_eq!(v["fr_FR"], "Bonjour");
        assert_eq!(v["de_DE"], serde_json::Value::Null);
        assert!(!v.as_object().unwrap().contains_key("en_US"));

        assert_eq!(delta("m.f", Some(&base), &Cell::Null).unwrap(), Cell::Null);
    }

    #[test]
    fn project_and_delta_refuse_foreign_cell_shapes() {
        let arr = Cell::Json(Arc::new(serde_json::json!(["x"])));
        let err = project("res.partner", "name", &arr, None).unwrap_err();
        assert!(
            err.message().contains("res.partner.name"),
            "{}",
            err.message()
        );
        assert!(err.message().contains("array"), "{}", err.message());
        let err = project("res.partner", "name", &Cell::Int(1), None).unwrap_err();
        assert!(
            err.message().contains("res.partner.name"),
            "{}",
            err.message()
        );
        let err = delta("res.partner.name", None, &Cell::Int(1)).unwrap_err();
        assert!(
            err.message().contains("res.partner.name"),
            "{}",
            err.message()
        );
        let err = delta("res.partner.name", None, &arr).unwrap_err();
        assert!(err.message().contains("array"), "{}", err.message());
    }
}

//! Preserve every original base1.0 currency XML-ID/name/record-ID binding.

use serde::Deserialize;
use tokio_postgres::Client;

#[derive(Deserialize)]
struct OriginalIdentity {
    xmlid: String,
    name: String,
}

pub async fn snapshot(client: &Client) -> Vec<(String, String, i64)> {
    let expected: Vec<OriginalIdentity> = serde_json::from_str(include_str!(
        "../fixtures/base-catalog/original-identities.json"
    ))
    .unwrap();
    let names: Vec<&str> = expected.iter().map(|row| row.xmlid.as_str()).collect();
    let rows = client.query("SELECT d.name,c.name,c.id FROM ir_model_data d JOIN res_currency c ON c.id=d.res_id WHERE d.module='base' AND d.model='res.currency' AND d.name=ANY($1) ORDER BY d.name COLLATE \"C\"", &[&names]).await.unwrap();
    let actual: Vec<(String, String, i64)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    let expected_names: Vec<(&str, &str)> = expected
        .iter()
        .map(|r| (r.xmlid.as_str(), r.name.as_str()))
        .collect();
    assert_eq!(
        actual
            .iter()
            .map(|(xmlid, name, _)| (xmlid.as_str(), name.as_str()))
            .collect::<Vec<_>>(),
        expected_names,
        "all original currency names and external identifiers"
    );
    actual
}

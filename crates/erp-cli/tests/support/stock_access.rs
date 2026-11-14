// SPDX-License-Identifier: MIT OR Apache-2.0
//! Build the public refusal diagnostic from installed group labels. Behavioral permissions
//! are asserted independently in each caller.
use erp_orm::{Operation, Orm, SUPERUSER_ID};

pub async fn refusal(
    orm: &Orm,
    verb: &str,
    letter: &str,
    description: &str,
    model: &str,
) -> String {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query("WITH RECURSIVE allowing(id) AS (SELECT a.group_id FROM ir_access a JOIN ir_model m ON m.id=a.model_id WHERE m.model=$1 AND a.active AND a.domain IS NULL AND position($2 in a.operation)>0 UNION SELECT r.gid FROM res_groups_implied_rel r JOIN allowing a ON a.id=r.hid) SELECT DISTINCT p.sequence,p.name,g.sequence,g.name,g.id FROM allowing a JOIN res_groups g ON g.id=a.id LEFT JOIN res_groups_privilege p ON p.id=g.privilege_id ORDER BY p.sequence,p.name,g.sequence,g.name,g.id", &[&model, &letter]).await.unwrap();
    let labels = rows
        .iter()
        .map(|row| {
            let privilege: Option<String> = row.get(1);
            let group: String = row.get(3);
            match privilege {
                Some(p) => format!("{p} / {group}"),
                None => group,
            }
        })
        .collect::<Vec<_>>();
    tx.rollback().await.unwrap();
    let (operation, expected_verb) = match letter {
        "r" => (Operation::Read, "access"),
        "c" => (Operation::Create, "create"),
        "u" => (Operation::Write, "modify"),
        "d" => (Operation::Unlink, "delete"),
        other => panic!("unexpected test operation {other}"),
    };
    assert_eq!(verb, expected_verb);
    let specification = orm.registry().model(model).unwrap();
    assert_eq!(specification.description, description);
    // Policy outcomes and allowing groups are checked independently; the existing core
    // renderer owns the full diagnostic wording. Do not duplicate its messages here.
    erp_security::explain::model_message(specification, operation, &labels)
}

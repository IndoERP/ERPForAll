// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;

#[test]
fn operation_slots_bind_retries_to_the_original_payload() {
    let request = Uuid::from_bytes([3; 16]);
    let mut first = Slots::default();
    let first_key = first
        .key(
            request,
            "crm.lead.action_create_quotation",
            br#"{"ids":[4]}"#,
        )
        .unwrap();
    assert_eq!(
        first_key,
        first
            .key(
                request,
                "crm.lead.action_create_quotation",
                br#"{"ids":[4]}"#
            )
            .unwrap()
    );
    let next_key = first
        .key(request, "sale.order.action_confirm", br#"{"ids":[8]}"#)
        .unwrap();
    assert_ne!(first_key, next_key);
    let mut retry = Slots::default();
    assert_eq!(
        first_key,
        retry
            .key(
                request,
                "crm.lead.action_create_quotation",
                br#"{"ids":[99]}"#
            )
            .unwrap()
    );
    assert_ne!(
        first_key,
        Slots::default()
            .key(
                Uuid::from_bytes([4; 16]),
                "crm.lead.action_create_quotation",
                br#"{"ids":[4]}"#
            )
            .unwrap()
    );
}

#[test]
fn action_arguments_admit_only_discovered_typed_business_inputs() {
    assert!(arguments(r#"{"ids":[1],"context":{"allowed_company_ids":[2]}}"#).is_err());
    assert!(arguments(r#"{"ids":[1],"ids":[2]}"#).is_err());
    assert!(arguments(r#"{"ids": [-1]}"#).is_err());
    assert!(arguments(r#"{"ids": [1,1]}"#).is_err());
    assert!(arguments(r#"{"__method__":"write"}"#).is_err());
    assert_eq!(arguments(r#"{"ids":[3,8]}"#).unwrap().ids, vec![3, 8]);
}

#[test]
fn untrusted_tool_output_cannot_expand_write_intent() {
    let mut catalog = Catalog::default();
    catalog.observe("sale.order.action_confirm", false);
    catalog.seal();
    catalog.observe("sale.quotation.send.action_send_mail", false);
    catalog.observe("crm.lead.action_view_quotations", true);
    assert!(catalog.read_only("sale.order.action_confirm").is_ok());
    assert!(
        catalog
            .read_only("sale.quotation.send.action_send_mail")
            .is_err()
    );
    assert!(catalog.read_only("crm.lead.action_view_quotations").is_ok());
}

#[test]
fn public_receipts_only_expose_navigation_targets() {
    let mut receipt = json!({"status":"completed","result":{"type":"ir.actions.client","tag":"display_notification",
        "source_record":{"res_model":"sale.order","res_id":7},"params":{"message":"private note"}}});
    assert_eq!(
        public_receipt(receipt.clone())["result"],
        json!({"res_model":"sale.order","res_id":7})
    );
    receipt["result"] = json!({"res_model":"sale.order","res_id":7,"data":"private note"});
    assert!(public_receipt(receipt).get("result").is_none());
}

#[test]
fn write_verification_has_no_tools_and_keeps_proposals_out_of_user_instructions() {
    let action = Action {
        name: "sale.order.action_confirm".into(),
        args: arguments(r#"{"ids":[5]}"#).unwrap(),
        read_only: false,
    };
    let req = action.intent_request("test", "Show quotation 5").unwrap();
    assert!(req.tools().is_empty());
    assert_eq!(req.messages().len(), 2);
    assert_eq!(req.messages()[0].role(), TurnRole::System);
    let [ContentPart::UserText(user), ContentPart::Data(proposal)] = req.messages()[1].parts()
    else {
        panic!("separate user intent and proposed operation")
    };
    assert_eq!(user.as_str(), "Show quotation 5");
    assert_eq!(proposal.value()["action"], "sale.order.action_confirm");
}

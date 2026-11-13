// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "test assertions")]
use erp_addon_barcodes::engine::Nomenclature;
use serde_json::Value;
#[test]
fn supported_epc_uri_and_malformed_inputs() {
    let n = Nomenclature::from_json(r#"{"version":1,"conversion":"none","rules":[]}"#).unwrap();
    for (uri, product, lot) in [
        (
            "urn:epc:class:lgtin : 4012345.012345.998877",
            "04012345123456",
            "998877",
        ),
        (
            "urn:epc:id:sgtin:9521141.012345.4711",
            "09521141123454",
            "4711",
        ),
        (
            "urn:epc:tag:sgtin-96:1.358378.0728089.620776",
            "03583787280898",
            "620776",
        ),
        (
            "urn:epc:tag:sgtin-198:1.358378.0728089.ABC",
            "03583787280898",
            "ABC",
        ),
    ] {
        let r: Value = serde_json::from_str(&n.parse_json(uri).unwrap()).unwrap();
        assert_eq!(r["status"], "uri");
        assert_eq!(r["items"][0]["type"], "product");
        assert_eq!(r["items"][0]["value"], product);
        assert_eq!(r["items"][1]["value"], lot);
        assert_eq!(r["items"][1]["type"], "lot");
        assert_eq!(r["items"][0]["base_code"], uri);
    }
    for uri in [
        "urn:epc:id:sscc:952656789012.03456",
        "urn:epc:tag:sscc-96:1.952656789012.03456",
    ] {
        let r: Value = serde_json::from_str(&n.parse_json(uri).unwrap()).unwrap();
        assert_eq!(r["items"].as_array().unwrap().len(), 1);
        assert_eq!(r["items"][0]["type"], "package");
        assert_eq!(r["items"][0]["value"], "095265678901234568");
    }
    for uri in [
        "urn:epc:id:sgtin:1.2",
        "urn:epc:id:sgtin:abc.012345.1",
        "urn:epc:tag:sgtin-96:9.358378.0728089.1",
        "urn:epc:id:sscc:1.2.3",
    ] {
        let e = n.parse_json(uri).unwrap_err();
        assert_eq!(e.kind(), "uri");
        assert_eq!(e.to_string(), "Invalid EPC URI components.");
    }
    let e = n.parse_json("urn:epc:id:unknown:123").unwrap_err();
    assert_eq!(e.kind(), "unsupported");
    assert_eq!(e.to_string(), "Unsupported EPC URI identifier.");
}

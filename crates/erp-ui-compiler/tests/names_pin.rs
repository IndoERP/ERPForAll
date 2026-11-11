//! `AddonName` accepts exactly the names `erp_modules::manifest::ModuleName` accepts (type-design
//! review TD-M6): bundle assembly converts installed modules to addon names, and that conversion
//! must never fail for a valid module.

#![allow(clippy::unwrap_used, reason = "tests")]

use erp_modules::manifest::ModuleName;
use erp_ui_compiler::AddonName;
use proptest::prelude::*;

fn agree(name: &str) {
    assert_eq!(
        AddonName::new(name).is_ok(),
        ModuleName::validate(name).is_ok(),
        "AddonName and ModuleName disagree on {name:?}"
    );
}

#[test]
fn edges_agree() {
    for name in [
        "",
        "web",
        "Web_2",
        "web-x",
        "web.x",
        "wéb",
        " web",
        "web\n",
        &"a".repeat(255),
        &"a".repeat(256),
        &"a".repeat(257),
    ] {
        agree(name);
    }
    assert!(AddonName::new(&"a".repeat(256)).is_ok());
    assert!(AddonName::new(&"a".repeat(257)).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn names_agree(name in "[A-Za-z0-9_.\\- é]{0,300}") {
        agree(&name);
    }

    #[test]
    fn arbitrary_strings_agree(name in any::<String>()) {
        agree(&name);
    }
}

//! The `res.partner` form fixture, shared by the criterion bench
//! (`benches/views_combine.rs`) and the evaluation-budget test (`tests/work_limits.rs`).

use erp_views::ViewDescriptor;

fn field(out: &mut String, name: &str, extra: &str) {
    out.push_str(&format!(
        r#"<field name="{name}" {extra} invisible="not is_company and type == 'contact' and parent_id" readonly="type == 'contact' and parent_id" options="{{'no_create': True, 'no_open': True}}" context="{{'default_type': 'contact', 'show_address': 1}}"/>"#
    ));
}

fn base_arch() -> String {
    let mut a = String::from(r#"<form string="Partners"><header>"#);
    for i in 0..4 {
        a.push_str(&format!(
            r#"<button name="action_header_{i}" type="object" string="Header {i}" invisible="not active"/>"#
        ));
    }
    a.push_str(r#"</header><sheet><div class="oe_button_box" name="button_box">"#);
    for i in 0..6 {
        a.push_str(&format!(
            r#"<button name="action_view_{i}" type="object" class="oe_stat_button" icon="fa-list"><field name="count_{i}" widget="statinfo" string="Items {i}"/></button>"#
        ));
    }
    a.push_str(r#"</div><widget name="web_ribbon" title="Archived" bg_color="text-bg-danger" invisible="active"/><div class="oe_title mb24"><field name="is_company" invisible="1"/><h1>"#);
    field(
        &mut a,
        "name",
        r#"default_focus="1" placeholder="e.g. Brandom Freeman""#,
    );
    a.push_str(r#"</h1><div class="o_row">"#);
    field(
        &mut a,
        "parent_id",
        r#"widget="res_partner_many2one" placeholder="Company Name...""#,
    );
    a.push_str(r#"</div></div><group name="main"><group name="address">"#);
    a.push_str(r#"<span class="o_form_label o_td_label" name="address_name"><b>Address</b></span><div class="o_address_format">"#);
    for name in ["street", "street2", "city", "state_id", "zip", "country_id"] {
        field(&mut a, name, r#"class="o_address_field""#);
    }
    a.push_str("</div>");
    for name in ["vat", "company_registry", "industry_id"] {
        field(&mut a, name, "");
    }
    a.push_str(r#"</group><group name="contact">"#);
    for name in [
        "function",
        "phone",
        "mobile",
        "user_ids",
        "email",
        "website",
        "title",
        "lang",
        "category_id",
    ] {
        field(&mut a, name, r#"widget="char""#);
    }
    a.push_str(r#"</group></group><notebook colspan="4">"#);
    for (page, fields) in [
        ("contact_addresses", ["child_ids", "type", "comment", "ref"]),
        (
            "sales_purchases",
            ["user_id", "team_id", "property_payment_term_id", "barcode"],
        ),
        (
            "accounting",
            [
                "bank_ids",
                "property_account_receivable_id",
                "property_account_payable_id",
                "credit_limit",
            ],
        ),
        (
            "internal_notes",
            ["note", "signup_url", "partner_share", "employee_ids"],
        ),
    ] {
        a.push_str(&format!(
            r#"<page name="{page}" string="Page {page}"><group name="{page}_group">"#
        ));
        for name in fields {
            field(&mut a, name, "");
        }
        a.push_str("</group></page>");
    }
    a.push_str(r#"</notebook></sheet><chatter/></form>"#);
    a
}

fn extension(i: i64) -> (String, i32) {
    let kind = i % 10;
    let arch = match kind {
        0 => format!(
            r#"<xpath expr="//field[@name='email']" position="after"><field name="custom_{i}" widget="email" invisible="not email"/></xpath>"#
        ),
        1 => format!(
            r#"<field name="vat" position="attributes"><attribute name="placeholder">VAT {i}</attribute><attribute name="readonly" add="parent_id" separator="or"/></field>"#
        ),
        2 => format!(
            r#"<div name="button_box" position="inside"><button name="action_stat_{i}" type="object" class="oe_stat_button" icon="fa-star"><field name="stat_{i}" widget="statinfo"/></button></div>"#
        ),
        3 => format!(
            r#"<xpath expr="//page[@name='internal_notes']" position="before"><page name="page_{i}" string="Page {i}"><group><field name="page_field_{i}"/><field name="page_other_{i}"/></group></page></xpath>"#
        ),
        4 => r#"<data><xpath expr="//field[@name='function']" position="attributes"><attribute name="invisible" add="is_company" separator="or"/></xpath><xpath expr="//div[hasclass('o_address_format')]" position="attributes"><attribute name="class" add="o_extra" separator=" "/></xpath></data>"#
            .to_owned(),
        5 => format!(
            r#"<field name="phone" position="before"><field name="phone_sanitized_{i}" invisible="1"/></field>"#
        ),
        6 => format!(
            r#"<xpath expr="//group[@name='contact']/field[@name='website']" position="after"><field name="social_{i}" widget="url"/></xpath>"#
        ),
        7 => format!(
            r#"<xpath expr="//page[@name='contact_addresses']//field[@name='child_ids']" position="replace"><div class="wrap_{i}">$0</div></xpath>"#
        ),
        8 => format!(
            r#"<xpath expr="//group[@name='main']" position="after"><group name="extra_{i}"><field name="extra_{i}"/><xpath expr="//field[@name='lang']" position="move"/></group></xpath>"#
        ),
        _ => format!(
            r#"<xpath expr="//header" position="inside"><button name="action_ext_{i}" type="object" string="Ext {i}" class="oe_highlight"/></xpath>"#
        ),
    };
    (arch, 10 + (i % 7) as i32)
}

/// The base view (id 1) and its 40 extensions (ids 2 to 41).
pub fn fixture() -> Vec<ViewDescriptor> {
    let mut views = vec![ViewDescriptor::new(
        1,
        "base.view_partner_form",
        "res.partner",
        base_arch(),
    )];
    for i in 2..=41 {
        let (arch, priority) = extension(i);
        views.push(
            ViewDescriptor::new(
                i,
                format!("addon_{i}.view_partner_form"),
                "res.partner",
                arch,
            )
            .extension_of(1)
            .with_priority(priority),
        );
    }
    views
}

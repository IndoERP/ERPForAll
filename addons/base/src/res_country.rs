//! `res.country`, `res.country.state`, and `res.country.group` models.

use erp_sdk::prelude::*;

/// `res.country`.
#[model(
    name = "res.country",
    description = "Country",
    order = "name, id",
    constraints = [
        (
            "_name_uniq",
            "unique (name)",
            "The name of the country must be unique!"
        ),
        (
            "_code_uniq",
            "unique (code)",
            "The code of the country must be unique!"
        ),
        (
            "_code_check",
            "CHECK (length(code) = 2 and upper(code) = code)",
            "The code of the country must be 2 letters and uppercase."
        ),
    ],
)]
pub struct ResCountry {
    /// Country name.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Country code (ISO 3166-1 alpha-2, 2 chars uppercase).
    #[field(char, size = 2, required = true, index = true)]
    pub code: Option<Arc<str>>,
    /// Display format for addresses belonging to this country.
    #[field(
        text,
        default = "%(street)s\n%(street2)s\n%(city)s %(state_code)s %(zip)s\n%(country_name)s"
    )]
    pub address_format: Option<Arc<str>>,
    /// Custom address input view.
    #[field(many2one, comodel = "ir.ui.view")]
    pub address_view_id: Option<RecId>,
    /// Currency of the country.
    #[field(many2one, comodel = "res.currency")]
    pub currency_id: Option<RecId>,
    /// International phone prefix.
    #[field(integer)]
    pub phone_code: Option<i32>,
    /// Customer Name Position ('before' or 'after').
    #[field(
        selection,
        selection = [("before", "Before Address"), ("after", "After Address")],
        default = "before"
    )]
    pub name_position: Option<Arc<str>>,
    /// VAT / Tax ID label.
    #[field(char)]
    pub vat_label: Option<Arc<str>>,
    /// State required on address.
    #[field(boolean, default = false)]
    pub state_required: bool,
    /// Zip code applicability.
    #[field(
        selection,
        selection = [
            ("required", "Required"),
            ("optional", "Optional"),
            ("not_applicable", "Not Applicable")
        ],
        default = "required",
        required = true
    )]
    pub zip_applicability: Option<Arc<str>>,
    /// Country groups this country belongs to.
    #[field(
        many2many,
        comodel = "res.country.group",
        relation = "res_country_res_country_group_rel",
        column1 = "res_country_id",
        column2 = "res_country_group_id"
    )]
    pub country_group_ids: Vec<RecId>,
    /// Federal states belonging to this country.
    #[field(one2many, comodel = "res.country.state", inverse_name = "country_id")]
    pub state_ids: Vec<RecId>,
}

#[methods(ResCountry, addon = "base")]
impl ResCountry {
    fn sanitize_vals(vals: &mut RawVals) {
        if let Some(Cell::Str(code)) = vals.get("code") {
            let upper = code.to_uppercase();
            if upper != code.as_ref() {
                vals.insert("code".into(), Cell::Str(upper.into()));
            }
        }
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ResCountry>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for vals in &mut vals_list {
            Self::sanitize_vals(vals);
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ResCountry>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        Self::sanitize_vals(&mut vals);
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method]
    async fn get_address_fields(
        this: &RecordSet<ResCountry>,
        tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        this.ensure_one()?;
        let format = this.mapped(tx, ResCountry::ADDRESS_FORMAT).await?;
        let Some(format_str) = format.into_iter().next().flatten() else {
            return Ok(Vec::new());
        };
        let mut fields = Vec::new();
        let mut rest = format_str.as_ref();
        while let Some(start) = rest.find("%(") {
            let after = &rest[start + 2..];
            if let Some(end) = after.find(')') {
                let key = &after[..end];
                if !fields.iter().any(|f: &String| f == key) {
                    fields.push(key.to_string());
                }
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
        Ok(fields)
    }
}

/// `res.country.state`.
#[model(
    name = "res.country.state",
    description = "Country state",
    order = "code, id",
    rec_names_search = ["name", "code"],
    constraints = [
        (
            "_name_code_uniq",
            "unique (country_id, code)",
            "The code of the state must be unique by country!"
        ),
    ],
)]
pub struct ResCountryState {
    /// State name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// State code.
    #[field(char, required = true, index = true)]
    pub code: Option<Arc<str>>,
    /// Country the state belongs to.
    #[field(
        many2one,
        comodel = "res.country",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    pub country_id: Option<RecId>,
}

#[methods(ResCountryState, addon = "base")]
impl ResCountryState {}

/// `res.country.group`.
#[model(
    name = "res.country.group",
    description = "Country Group",
    order = "name, id",
    constraints = [
        (
            "_check_code_uniq",
            "unique (code)",
            "The country group code must be unique!"
        ),
    ],
)]
pub struct ResCountryGroup {
    /// Group name.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Group code.
    #[field(char, index = true)]
    pub code: Option<Arc<str>>,
    /// Member countries.
    #[field(
        many2many,
        comodel = "res.country",
        relation = "res_country_res_country_group_rel",
        column1 = "res_country_group_id",
        column2 = "res_country_id"
    )]
    pub country_ids: Vec<RecId>,
}

#[methods(ResCountryGroup, addon = "base")]
impl ResCountryGroup {
    fn sanitize_vals(vals: &mut RawVals) {
        if let Some(Cell::Str(code)) = vals.get("code") {
            let upper = code.to_uppercase();
            if upper != code.as_ref() {
                vals.insert("code".into(), Cell::Str(upper.into()));
            }
        }
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ResCountryGroup>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for vals in &mut vals_list {
            Self::sanitize_vals(vals);
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ResCountryGroup>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        Self::sanitize_vals(&mut vals);
        sup.call(tx, this.as_records(), (vals,)).await
    }
}

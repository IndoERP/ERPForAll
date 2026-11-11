//! Product Category model (`product.category`).

use erp_sdk::prelude::*;

/// `product.category`: Hierarchical classification of products.
#[model(
    name = "product.category",
    description = "Product Category",
    parent_store = true,
    parent_name = "parent_id",
    order = "name, id",
    rec_name = "complete_name",
    rec_names_search = ["complete_name", "name"],
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow"),
)]
pub struct ProductCategory {
    /// Category name.
    #[field(char, required = true, translate = true, index = "trigram")]
    pub name: Option<Arc<str>>,

    /// Complete hierarchical name ('Parent / Child').
    #[field(char, compute = "_compute_complete_name")]
    pub complete_name: Option<Arc<str>>,

    /// Parent category in tree.
    #[field(
        many2one,
        comodel = "product.category",
        ondelete = "cascade",
        index = true
    )]
    pub parent_id: Option<RecId>,

    /// Subcategories.
    #[field(one2many, comodel = "product.category", inverse_name = "parent_id")]
    pub child_id: Vec<RecId>,

    /// Materialized ancestry path managed by the ORM parent store.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,

    /// Count of product templates in this category and all descendants.
    #[field(integer, compute = "_compute_product_count")]
    pub product_count: i32,

    /// Restricting company (None means shared across companies).
    #[field(many2one, comodel = "res.company", index = true)]
    pub company_id: Option<RecId>,
}

#[methods(ProductCategory, addon = "product")]
impl ProductCategory {
    /// Compute recursive complete_name ('Parent / Child').
    #[compute(depends = ["name", "parent_id.complete_name"], depends_context = ["hierarchical_naming"])]
    async fn _compute_complete_name(
        this: &RecordSet<ProductCategory>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductCategory>,
    ) -> Result<()> {
        let hierarchical = this
            .env()
            .context()
            .get("hierarchical_naming")
            .map(|v| v.is_truthy())
            .unwrap_or(true);

        let names = this.mapped(tx, ProductCategory::NAME).await?;
        let parent_ids = this.mapped(tx, ProductCategory::PARENT_ID).await?;

        for (index, (name_opt, parent_opt)) in names.into_iter().zip(parent_ids).enumerate() {
            let name = name_opt.as_deref().unwrap_or("");
            if !hierarchical || parent_opt.is_none() {
                out.set(index, ProductCategory::COMPLETE_NAME, Some(name.into()))?;
            } else if let Some(parent_id) = parent_opt {
                let parent_records = this
                    .env()
                    .records("product.category")?
                    .browse(vec![parent_id.get()])?;
                let parent_set = RecordSet::<ProductCategory>::try_from(parent_records)?;
                let parent_comp: Vec<Option<Arc<str>>> = parent_set
                    .mapped(tx, ProductCategory::COMPLETE_NAME)
                    .await?;
                let parent_name = parent_comp
                    .into_iter()
                    .next()
                    .flatten()
                    .as_deref()
                    .unwrap_or("")
                    .to_string();
                let full = if parent_name.is_empty() {
                    name.to_string()
                } else {
                    format!("{parent_name} / {name}")
                };
                out.set(index, ProductCategory::COMPLETE_NAME, Some(full.into()))?;
            }
        }
        Ok(())
    }

    /// Compute product_count: templates in this category and all descendants.
    #[compute(depends = ["child_id"])]
    async fn _compute_product_count(
        this: &RecordSet<ProductCategory>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductCategory>,
    ) -> Result<()> {
        let ids = this.as_records().rec_ids();
        if ids.is_empty() {
            return Ok(());
        }

        let paths: Vec<Option<Arc<str>>> = this.mapped(tx, ProductCategory::PARENT_PATH).await?;
        let templates = this.env().records("product.template")?;

        for (index, path_opt) in paths.into_iter().enumerate() {
            let id = ids[index].get();
            let domain = if let Some(path) = path_opt {
                Domain::condition("categ_id.parent_path", Operator::Like, format!("{path}%"))?
            } else {
                Domain::condition("categ_id", Operator::Eq, id)?
            };
            let count = templates.search_count(tx, &domain, None).await?;
            let count = i32::try_from(count).map_err(|_| {
                Error::programming(format!("product count {count} does not fit an i32"))
            })?;
            out.set(index, ProductCategory::PRODUCT_COUNT, count)?;
        }
        Ok(())
    }

    /// Multi-company constraint.
    #[constrains("company_id")]
    async fn _check_company_id(
        this: &RecordSet<ProductCategory>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let company_ids = this.mapped(tx, ProductCategory::COMPANY_ID).await?;
        let tmpl_model = this.env().records("product.template")?;

        for (record_id, comp_opt) in this.as_records().rec_ids().iter().zip(company_ids) {
            if let Some(comp_id) = comp_opt {
                let d1 = Domain::condition("categ_id", Operator::ChildOf, record_id.get())?;
                let d2 = Domain::condition("company_id", Operator::Ne, comp_id.get())?;
                let count = tmpl_model.search_count(tx, &(d1 & d2), None).await?;
                if count > 0 {
                    return Err(Error::validation(
                        "You cannot change the company of a category that is used by products of another company.",
                    ));
                }
            }
        }
        Ok(())
    }
}

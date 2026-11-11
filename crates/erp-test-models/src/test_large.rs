//! Large-table schema sync fixtures: `test_large_base` declares
//! `test_large.item`; `test_large_ext` extends it with work that would hold heavy locks on a table
//! with many rows — field indexes, a foreign key, a unique index, columns that become required and
//! a new stored computed column. `test_large_index` and `test_large_required` each make one change
//! whose whole work is a step on a large table (an index; a column made required with a default);
//! `test_large_key` adds a business key, built by two steps on a large table.

/// `test_large.item` before the extension.
pub mod test_large_base {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_large_base");

    /// `test_large.item`.
    #[model(
        name = "test_large.item",
        description = "Large Item",
        addon = "test_large_base"
    )]
    pub struct Item {
        /// Title.
        #[field(char)]
        pub name: Option<Arc<str>>,
        /// Quantity; becomes required with a default in `test_large_ext`.
        #[field(integer)]
        pub qty: i32,
        /// Reference; becomes required without a default in `test_large_ext`.
        #[field(char)]
        pub reference: Option<Arc<str>>,
    }
}

/// The extension of `test_large.item` whose schema change is large-table work.
pub mod test_large_ext {
    use erp_sdk::prelude::*;

    use super::test_large_base::Item;

    erp_sdk::addon!(name = "test_large_ext", depends = ["test_large_base"]);

    /// New indexed, relational and computed fields, and two fields made required.
    #[extend(
        Item,
        addon = "test_large_ext",
        unique_indexes = [("_code_uniq", "(code)", "the code must be unique")]
    )]
    pub struct ItemExt {
        /// An indexed code (field index and table-object unique index).
        #[field(char, index)]
        pub code: Option<Arc<str>>,
        /// A many2one: foreign key and index.
        #[field(many2one, comodel = "res.users")]
        pub owner_id: Option<RecId>,
        /// Length of the name, stored: existing rows are backfilled.
        #[field(integer, compute = "_compute_name_size", store)]
        pub name_size: i32,
        /// Redefined: now required, with a default (existing NULLs are filled).
        #[field(integer, required, default = 7)]
        pub qty: i32,
        /// Redefined: now required, without a default.
        #[field(char, required)]
        pub reference: Option<Arc<str>>,
    }

    #[methods(Item, addon = "test_large_ext")]
    impl ItemExt {
        #[compute(depends = ["name"])]
        fn _compute_name_size(batch: &mut Batch<'_, Item>) -> Result<()> {
            for i in batch.indices() {
                let name = batch.get(i, Item::NAME)?;
                let size = name.as_deref().map_or(0, |n| n.chars().count());
                let size = i32::try_from(size).map_err(|_| {
                    Error::validation(format!("name of {} characters is too long", size))
                })?;
                batch.set(i, ItemExt::NAME_SIZE, size)?;
            }
            Ok(())
        }
    }
}

/// One change on `test_large.item`: an index on an existing column. On a large table the index is
/// built by a step, so the schema transaction runs no DDL.
pub mod test_large_index {
    use erp_sdk::prelude::*;

    use super::test_large_base::Item;

    erp_sdk::addon!(name = "test_large_index", depends = ["test_large_base"]);

    /// `name`, now indexed.
    #[extend(Item, addon = "test_large_index")]
    pub struct ItemIndexed {
        /// Title, indexed.
        #[field(char, index)]
        pub name: Option<Arc<str>>,
    }
}

/// One change on `test_large.item`: an existing column made required with a default. On a large
/// table the fill, the constraint and its validation are a step, so the schema transaction runs no
/// DDL.
pub mod test_large_required {
    use erp_sdk::prelude::*;

    use super::test_large_base::Item;

    erp_sdk::addon!(name = "test_large_required", depends = ["test_large_base"]);

    /// `qty`, now required with a default.
    #[extend(Item, addon = "test_large_required")]
    pub struct ItemRequired {
        /// Quantity, required, 7 by default.
        #[field(integer, required, default = 7)]
        pub qty: i32,
    }
}

/// A second module adding a stored computed field to `test_large.item`: with `test_large_ext`
/// installed first, the backfill the queue holds for the table grows from one field to two.
pub mod test_large_ext2 {
    use erp_sdk::prelude::*;

    use super::test_large_base::Item;

    erp_sdk::addon!(name = "test_large_ext2", depends = ["test_large_base"]);

    /// Twice the length of the name, stored: existing rows are backfilled.
    #[extend(Item, addon = "test_large_ext2")]
    pub struct ItemExt2 {
        /// Twice the length of the name.
        #[field(integer, compute = "_compute_name_size2", store)]
        pub name_size2: i32,
    }

    #[methods(Item, addon = "test_large_ext2")]
    impl ItemExt2 {
        #[compute(depends = ["name"])]
        fn _compute_name_size2(batch: &mut Batch<'_, Item>) -> Result<()> {
            for i in batch.indices() {
                let name = batch.get(i, Item::NAME)?;
                let size = name.as_deref().map_or(0, |n| n.chars().count());
                let size = i32::try_from(size * 2).map_err(|_| {
                    Error::validation(format!("name of {} characters is too long", size))
                })?;
                batch.set(i, ItemExt2::NAME_SIZE2, size)?;
            }
            Ok(())
        }
    }
}

/// A required stored computed field on `test_large.item`: installed on an empty table (a new
/// required column without default is refused on a table with rows), it lets a test reproduce a
/// backfill that leaves NULLs in a required field.
pub mod test_large_req {
    use erp_sdk::prelude::*;

    use super::test_large_base::Item;

    erp_sdk::addon!(name = "test_large_req", depends = ["test_large_base"]);

    /// Three times the length of the name, stored and required.
    #[extend(Item, addon = "test_large_req")]
    pub struct ItemReq {
        /// Three times the length of the name.
        #[field(integer, compute = "_compute_name_size3", store, required)]
        pub name_size3: i32,
    }

    #[methods(Item, addon = "test_large_req")]
    impl ItemReq {
        #[compute(depends = ["name"])]
        fn _compute_name_size3(batch: &mut Batch<'_, Item>) -> Result<()> {
            for i in batch.indices() {
                let name = batch.get(i, Item::NAME)?;
                let size = name.as_deref().map_or(0, |n| n.chars().count());
                let size = i32::try_from(size * 3).map_err(|_| {
                    Error::validation(format!("name of {} characters is too long", size))
                })?;
                batch.set(i, ItemReq::NAME_SIZE3, size)?;
            }
            Ok(())
        }
    }
}

/// A business key on `test_large.item` over `reference`: on a large table it is
/// built after the commit in two steps (the unique index, then the constraint adopting it), which a
/// module operation queues like the other large-table work.
pub mod test_large_key {
    use erp_sdk::prelude::*;

    use super::test_large_base::Item;

    erp_sdk::addon!(name = "test_large_key", depends = ["test_large_base"]);

    /// The key over `reference`.
    #[extend(
        Item,
        addon = "test_large_key",
        keys = [key(
            name = "_reference_key",
            fields = ["reference"],
            message = "An item with this reference already exists."
        )]
    )]
    pub struct ItemKey {}
}

//! Partitioned tables above the large-table threshold (design §7.10): the base addon
//! declares two partitioned models; each extension makes one change that, on a table with rows,
//! is large-table work. `scale.big` is keyed on `create_date`, `scale.slot` on the server-set
//! `recorded_at`.

/// The two partitioned models before any extension.
pub mod test_partition_large_base {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_partition_large_base");

    /// `scale.big`: daily partitions on `create_date`, every partition kept.
    #[model(
        name = "scale.big",
        description = "Scale Big",
        addon = "test_partition_large_base",
        partition = partition(interval = "day", premake = 1, retain = "all")
    )]
    pub struct Big {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }

    /// `scale.slot`: monthly partitions on the server-set `recorded_at`, every partition kept.
    #[model(
        name = "scale.slot",
        description = "Scale Slot",
        addon = "test_partition_large_base",
        partition = partition(by = "recorded_at", interval = "month", premake = 1, retain = "all")
    )]
    pub struct Slot {
        #[field(datetime, required, server_set)]
        pub recorded_at: Option<DateTime>,
        #[field(integer)]
        pub resource: i32,
        #[field(datetime)]
        pub start: Option<DateTime>,
        #[field(datetime)]
        pub stop: Option<DateTime>,
    }
}

/// An indexed field and a unique index that includes the partition key.
pub mod test_partition_large_index {
    use erp_sdk::prelude::*;

    use super::test_partition_large_base::Big;

    erp_sdk::addon!(
        name = "test_partition_large_index",
        depends = ["test_partition_large_base"]
    );

    /// `code`, indexed, and unique together with `create_date`.
    #[extend(
        Big,
        addon = "test_partition_large_index",
        unique_indexes = [("_code_uniq", "(code, create_date)", "The code must be unique.")]
    )]
    pub struct BigIndexed {
        #[field(char, index)]
        pub code: Option<Arc<str>>,
    }
}

/// A `UNIQUE` constraint object.
pub mod test_partition_large_unique {
    use erp_sdk::prelude::*;

    use super::test_partition_large_base::Big;

    erp_sdk::addon!(
        name = "test_partition_large_unique",
        depends = ["test_partition_large_base"]
    );

    /// `UNIQUE (name, create_date)`.
    #[extend(
        Big,
        addon = "test_partition_large_unique",
        constraints = [("_name_uniq", "UNIQUE (name, create_date)", "The name must be unique.")]
    )]
    pub struct BigUnique {}
}

/// An exclusion that includes the partition key.
pub mod test_partition_large_excl {
    use erp_sdk::prelude::*;

    use super::test_partition_large_base::Slot;

    erp_sdk::addon!(
        name = "test_partition_large_excl",
        depends = ["test_partition_large_base"]
    );

    /// No two slots of a resource overlap within one recording time.
    #[extend(
        Slot,
        addon = "test_partition_large_excl",
        exclusions = [exclude(
            name = "_no_overlap",
            equal = ["recorded_at", "resource"],
            period = ("start", "stop"),
            bounds = "[)",
            message = "This resource is taken."
        )]
    )]
    pub struct SlotExcl {}
}

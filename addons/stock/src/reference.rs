// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock references (`stock.reference`): table only. A reference names the
//! documents (sale order, purchase order, ...) a set of moves belongs to (topology ADR D3). Keeping
//! the transfer union in step with the moves is's and's.

use erp_sdk::prelude::*;

/// `stock.reference`: a document reference shared by moves and their transfers.
///
/// No company field, so no company access entry (as the package).
/// Each table's primary key is D3's unique pair and its reverse index the second lookup
/// order. The other side of each table is declared on
/// the move ([`crate::StockMove::reference_ids`]) and the transfer
/// ([`crate::StockPicking::reference_ids`]).
#[model(
    name = "stock.reference",
    description = "Reference between stock documents"
)]
pub struct StockReference {
    /// Reference text, supplied by the document that creates it.
    #[field(char, required = true, readonly = true, string = "Reference")]
    pub name: Option<Arc<str>>,
    /// Moves of this reference.
    #[field(
        many2many,
        comodel = "stock.move",
        relation = "stock_reference_move_rel",
        column1 = "reference_id",
        column2 = "move_id",
        string = "Stock Moves"
    )]
    pub move_ids: Vec<RecId>,
    /// `readonly` only marks it in the client: the ORM and RPC still
    /// write it until adds D3's refusal of direct writes.
    #[field(
        many2many,
        comodel = "stock.picking",
        relation = "stock_reference_picking_rel",
        column1 = "reference_id",
        column2 = "picking_id",
        readonly = true,
        string = "Transfers"
    )]
    pub picking_ids: Vec<RecId>,
}

use erp_views::{CheckedNode, CheckedViewInput};

fn escape<'a>(input: CheckedViewInput<'a>) -> CheckedNode<'a> {
    input.root().unwrap()
}

fn main() {}

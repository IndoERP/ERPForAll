use erp_views::CheckedViewInput;

fn projection_must_borrow_input(input: &CheckedViewInput<'_>) {
    let escaped;
    {
        let mobile = input.with_mobile(true);
        escaped = mobile.with_mobile(false);
    }
    let _ = escaped.mobile();
}

fn main() {}

//! Emit the single human UI contract from authoritative Rust declarations.
use erp_views::wire::*;
use std::{env, error::Error, fs, io};
use ts_rs::TS;
fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .ok_or_else(|| io::Error::other("usage: human_ui_schema OUTPUT_FILE"))?;
    if args.next().is_some() {
        return Err(io::Error::other("unexpected argument").into());
    }
    let mut text = String::from(
        "// Generated from erp-views::wire by human_ui_schema. Do not edit.\nimport type { WireDocument } from '../../../../../web/src/generated/WireDocument';\n",
    );
    macro_rules! emit { ($($t:ty),* $(,)?) => { $(text.push_str("export "); text.push_str(&<$t>::decl()); text.push('\n');)* }; }
    emit!(
        WireId,
        WirePageLimit,
        WireMajor,
        LoginPage,
        Bootstrap,
        Identity,
        Company,
        Locale,
        Csrf,
        Renderer,
        MenuTree,
        MenuNode,
        ActionTarget,
        ViewType,
        WindowAction,
        WindowActionViews,
        ActionView,
        ViewDescription,
        FieldKind,
        FieldDescription,
        ViewIdentity,
        View,
        FieldRef,
        Modifiers,
        EmptyOptions,
        StatusbarOptions,
        CodeMode,
        CodeOptions,
        FieldPresentationOptions,
        FieldOptions,
        RecordMode,
        ButtonType,
        ButtonVariant,
        ButtonDetails,
        ButtonSize,
        ButtonTarget,
        RibbonBackground,
        ScopeKind,
        SettingsOptions,
        WidgetOptions,
        ServiceWidget,
        PanelVersion,
        PanelDisplayLimit,
        PanelValue,
        PanelDomain,
        PanelSection,
        ListSearchPanel,
        ViewNode,
        NotificationLevel,
        ActionResult,
        ActionResultEnvelope,
        Notice,
        SettingsScope,
        SettingKind,
        SettingValue,
        SettingsDescription,
        SettingDescription,
        ValueSource,
        SettingsValues,
        SettingEntry,
        AddonState,
        AddonCatalog,
        AddonEntry,
        InstallResult,
        UpgradeResult,
        UninstallResult,
        AddonDetailLimit,
        AddonDetailRequest,
        AddonDetailState,
        AddonNamePage,
        AddonMenu,
        AddonView,
        AddonMenuPage,
        AddonViewPage,
        AddonDetail,
        WireDecimalCount
    );
    // ts-rs emits trailing spaces around documented enum fields; normalize the
    // generated artifact here so consumers do not hand-edit generated declarations.
    let text = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(path, text)?;
    Ok(())
}

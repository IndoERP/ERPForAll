/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
export const bootstrap = {
  wire_major: 1,
  registry_version: "r1",
  asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Company", parent_id: null }],
  current_company_id: 1,
  allowed_company_ids: [1],
  active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token-not-secret" }
};

export const description = {
  wire_major: 1,
  registry_version: "r1",
  description_version: "d1",
  model: "test.note",
  fields: {
    "test.note": {
      name: { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null }
    }
  },
  notices: [],
  views: [
    {
      identity: { kind: "stored", id: 1 },
      type: "form",
      schema_major: 1,
      renderer: "web.form",
      root: 0,
      nodes: [
        { kind: "form", record_mode: "record", children: [1] },
        { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }
      ]
    },
    {
      identity: { kind: "stored", id: 2 },
      type: "list",
      schema_major: 1,
      renderer: "web.list",
      root: 0,
      nodes: [
        { kind: "list", children: [1] },
        { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }
      ]
    }
  ]
};

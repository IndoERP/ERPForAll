/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

const addons = join(dirname(fileURLToPath(import.meta.url)), "../../addons");

function record(file: string, id: string): Element {
  const doc = new DOMParser().parseFromString(readFileSync(join(addons, file), "utf8"), "application/xml");
  expect(doc.querySelector("parsererror")).toBeNull();
  const node = doc.querySelector(`record[id="${id}"]`);
  if (!node) throw new Error(`Missing ${id} in ${file}`);
  return node;
}

test("Contacts overview counts use the native directory's company and individual filters", () => {
  const contacts = "contacts/views/contacts_views.xml";
  expect(record(contacts, "contacts_overview_action").querySelector('[name="search_view_id"]')?.getAttribute("ref")).toBe("base.partner_search");
  const dashboard = record(contacts, "contacts_overview_list");
  const search = record("base/data/partner_views.xml", "partner_search");
  for (const [filter, value] of [["companies", "True"], ["individuals", "False"]]) {
    expect(dashboard.querySelector(`dashboard > count[filter="${filter}"]`)).not.toBeNull();
    expect(search.querySelector(`filter[name="${filter}"]`)?.getAttribute("domain")).toBe(`[('is_company', '=', ${value})]`);
  }
});

test("Catalog counts use the activity and product type fields maintained by the product model", () => {
  const view = record("product/views/product_template_views.xml", "catalog_overview_list");
  const domains = Array.from(view.querySelectorAll("dashboard > count"), node => node.getAttribute("domain"));
  expect(domains).toEqual(["[('active', '=', True)]", "[('type', '=', 'consu')]", "[('type', '=', 'service')]", "[('type', '=', 'combo')]"]);
});

test("Employees overview groups the public directory by department and retains HR-only menu restrictions", () => {
  const action = record("hr/views/hr_employee_views.xml", "employee_overview_action");
  expect(action.querySelector('[name="res_model"]')?.textContent).toBe("hr.employee");
  expect(action.querySelector('[name="context"]')?.textContent).toBe("{'search_default_group_department': 1}");
  const menus = new DOMParser().parseFromString(readFileSync(join(addons, "hr/views/hr_menus.xml"), "utf8"), "application/xml");
  expect(menus.querySelector("#hr_employee_menu")?.getAttribute("groups")).toBe("hr.group_hr_user");
  expect(menus.querySelector("#hr_reporting_menu")?.getAttribute("groups")).toBe("hr.group_hr_user");
});

test("Purchase overview includes draft and sent RFQs and leaves orders of every state in scope", () => {
  const file = "purchase/views/purchase_views.xml";
  const action = record(file, "purchase_overview_action");
  expect(action.querySelector('[name="domain"], [name="context"]')).toBeNull();
  expect(action.querySelector('[name="view_id"]')?.getAttribute("ref")).toBe("purchase_order_list");
  expect(record(file, "purchase_order_list").querySelector('count[name="rfq"]')?.getAttribute("domain")).toBe("[('state', 'in', ['draft', 'sent'])]");
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { Field } from "../metadata.js";

export type FacetType = "field" | "filter" | "groupBy" | "favorite";

export interface SearchFacet {
  id: string;
  type: FacetType;
  title: string;
  values: string[];
  separator: string;
  icon?: string;
  fieldName?: string;
  operator?: string;
  domain?: unknown[];
  groupByField?: string;
  filterId?: string;
  optionId?: string;
  /** The declared group-by item (or `custom`) a `groupBy` facet stands for. */
  groupById?: string;
}

export interface AutocompleteItem {
  id: string;
  label: string;
  fieldName: string;
  fieldLabel: string;
  searchTerm: string;
  operator: string;
  isParent?: boolean;
  isChild?: boolean;
  parentId?: string;
  /** The value as the user reads it (a selection label, a related record's name). */
  display?: string;
}

export interface FilterItem {
  id: string;
  description: string;
  domain: unknown[];
  isActive: boolean;
  groupNumber?: number;
  fieldName?: string;
  custom?: { field: string; operator: string; value: string | number | boolean };
  type?: "filter" | "dateFilter";
  dateOptions?: { id: string; label: string; domain: unknown[]; isActive: boolean }[];
}

export interface GroupByItem {
  id: string;
  description: string;
  fieldName: string;
  isActive: boolean;
  /** Chosen with "Add Custom Group", not declared by the search view. */
  custom?: boolean;
}

/** The kinds a list can be grouped by in this client (date granularity and many2many arrive with slice 2). */
export const GROUPABLE_KINDS = ["boolean", "char", "integer", "selection", "many2one"] as const;
export type GroupableKind = typeof GROUPABLE_KINDS[number];
/** A field the server reports as groupable (`fields_get` attribute `groupable`), in a kind this client groups by. */
export interface GroupableField<K extends string = GroupableKind> {
  name: string;
  label: string;
  kind: K;
  selection: readonly (readonly [string, string])[] | null;
}
/** The active grouping, kept in memory so a list reopened from a record's breadcrumb is grouped again. */
export interface GroupByState { id: string; label: string; field: GroupableField }
/** What the Group By menu offers: the search view's declared items, the groupable fields and the declared items left out, each with its reason. */
export interface GroupByCatalogue {
  items: { id: string; description: string; fieldName: string; /** A date interval the item groups by (the pivot view only). */ interval?: string }[];
  fields: GroupableField[];
  skipped: { description: string; reason: string }[];
}

export interface FavoriteItem {
  id: string;
  name: string;
  domain: unknown[];
  groupBy: string[];
  isDefault: boolean;
  isShared: boolean;
  isActive: boolean;
}

export interface SearchModelOptions {
  model: string;
  fields: Map<string, Field>;
  viewFields?: Field[];
  userId?: number;
  companyId?: number;
  initialDomain?: unknown[];
  /** Records of `relation` whose name matches, for a many2one's expandable suggestions. */
  nameSearch?: (relation: string, name: string) => Promise<{ id: number; display_name: string }[]>;
  /** Present when the list can be grouped: loads the catalogue the Group By menu shows (once, on first use). */
  groupBy?: { load: () => Promise<GroupByCatalogue> };
}

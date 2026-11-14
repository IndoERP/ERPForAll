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
  /** Every grouping of a `groupBy` facet in the order chosen (`field` or `field:interval`); the first is `groupByField`. */
  groupByFields?: string[];
  groupByIds?: string[];
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

/** The scalar grouping kinds shared with legacy consumers; owners separately admit temporal fields. */
export const GROUPABLE_KINDS = ["boolean", "char", "integer", "selection", "many2one"] as const;
export type GroupableKind = typeof GROUPABLE_KINDS[number] | "date" | "datetime";
/** A field the server reports as groupable (`fields_get` attribute `groupable`), in a kind this client groups by. */
export interface GroupableField<K extends string = GroupableKind> {
  name: string;
  label: string;
  kind: K;
  selection: readonly (readonly [string, string])[] | null;
}
/** The intervals a date grouping may use, as the menus list them. */
export const GROUP_INTERVALS: readonly (readonly [string, string])[] = [["year", "Year"], ["quarter", "Quarter"], ["month", "Month"], ["week", "Week"], ["day", "Day"]];
/** What the Group By menu offers: the search view's declared items, the groupable fields and the declared items left out, each with its reason. */
export interface ListAggregate { field: string; kind: "integer" | "float" | "monetary"; op: "sum" | "avg" | "min" | "max"; currency?: string }
export interface GroupByCatalogue {
  items: { id: string; description: string; fieldName: string; /** A temporal interval declared by the search view. */ interval?: string }[];
  fields: GroupableField[];
  skipped: { description: string; reason: string }[];
  aggregates?: ListAggregate[];
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
  /** Additional server-admitted filters, supplied before URL restoration. */
  filters?: readonly FilterItem[];
  model: string;
  fields: Map<string, Field>;
  viewFields?: Field[];
  userId?: number;
  companyId?: number;
  initialDomain?: unknown[];
  /** Records of `relation` whose name matches, for a many2one's expandable suggestions. */
  nameSearch?: (relation: string, name: string) => Promise<{ id: number; display_name: string }[]>;
  /** Present when the list can be grouped: loads the catalogue the Group By menu shows (once, on first use). */
  groupBy?: {
    load: () => Promise<GroupByCatalogue>;
    /** The owner enables ordered dimensions and date intervals (lists and pivots). */
    multiple?: boolean;
    /** List state belongs in search_groupby; pivot keeps its separate address contract. */
    persist?: boolean;
    accept?: (params: URLSearchParams) => void;
    /** The most groupings a facet holds when `multiple` (the owner's own bound). */
    maxLevels?: number;
  };
}

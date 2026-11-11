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
}

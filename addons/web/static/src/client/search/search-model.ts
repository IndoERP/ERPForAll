/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { Field } from "../metadata.js";
import { ClientError } from "../transport.js";
import type { AutocompleteItem, FavoriteItem, FilterItem, GroupableField, GroupByCatalogue, GroupByItem, SearchFacet, SearchModelOptions } from "./types.js";
import { decodeListState, type ListGroupState } from "../list-group-state.js";
import { GROUP_INTERVALS } from "./types.js";

const MAX_PARAM = 2048;
const MAX_ITEMS = 32;
const MAX_VALUE = 256;
const operators = ["ilike", "=", "!=", ">", "<", ">=", "<="];
const searchableKinds: readonly string[] = ["char", "text", "integer", "selection", "many2one", "one2many", "many2many"];
function defaultOperator(field: Field | undefined): string {
  return field?.kind === "integer" || field?.kind === "selection" ? "=" : "ilike";
}
const groupingUnavailable = (): never => { throw new ClientError("unsupported", "Grouped lists are not available yet."); };
const groupingFromAddress = (): never => { throw new ClientError("unsupported", "Restoring a grouping from the address is not available yet."); };
const GROUP_BY_FACET = "groupby";
const favoritesUnavailable = (): never => { throw new ClientError("unsupported", "Saved favorites are not available yet."); };
const invalid = (label: string): never => { throw new ClientError("validation", `Invalid ${label}.`); };
function record(raw: unknown, keys: string[], label: string): Record<string, unknown> {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return invalid(label);
  if (Object.keys(raw).some(key => !keys.includes(key))) return invalid(label);
  return raw as Record<string, unknown>;
}
function items(raw: unknown, label: string): unknown[] {
  if (!Array.isArray(raw) || raw.length > MAX_ITEMS) return invalid(label);
  return raw;
}
function term(raw: unknown, label: string): string {
  if (typeof raw !== "string" || !raw.trim() || raw.length > MAX_VALUE) return invalid(label);
  return raw.trim();
}
/** Join complete prefix expressions, retaining implicit AND inside each operand. */
function orDomains(domains: unknown[][]): unknown[] {
  if (domains.length === 1) return domains[0]!;
  const result: unknown[] = Array(Math.max(0, domains.length - 1)).fill("|");
  for (const domain of domains) {
    let roots = 0;
    for (const token of domain) roots += token === "|" || token === "&" ? -1 : token === "!" ? 0 : 1;
    result.push(...Array(Math.max(0, roots - 1)).fill("&"), ...domain);
  }
  return result;
}

/** Bounded state for the human client's currently supported search subset.
 * Owners enable ordered grouping and temporal intervals; lists also persist grouping in the address.
 * Favorites and general search-arch interpretation remain milestone work.
 */
export class SearchModel {
  readonly model: string;
  readonly fields: Map<string, Field>;
  readonly viewFields: Field[];
  readonly userId: number;
  readonly companyId: number;
  private facets: SearchFacet[] = [];
  private filters: FilterItem[] = [];
  private declaredFilters: FilterItem[] = [];
  private listeners: (() => void)[] = [];
  private readonly initialDomain: unknown[];
  private sequence = 0;
  private mutating = false;
  private deferredNotification = false;
  private readonly nameSearch: SearchModelOptions["nameSearch"];
  private readonly groupByLoader: SearchModelOptions["groupBy"];
  private readonly multipleGroupBy: boolean;
  private readonly maxGroupLevels: number;
  private listState: ListGroupState | undefined;
  private catalogue: GroupByCatalogue | undefined;
  private catalogueLoad: Promise<void> | undefined;
  /** Names of the related records picked from suggestions, keyed `field:id`, for their facets. */
  private readonly recordNames = new Map<string, string>();

  constructor(options: SearchModelOptions) {
    this.addDeclaredFilters(options.filters ?? []);
    this.model = options.model;
    this.fields = options.fields;
    this.viewFields = options.viewFields ?? [...options.fields.values()];
    this.userId = options.userId ?? 0;
    this.companyId = options.companyId ?? 0;
    this.initialDomain = options.initialDomain ?? [];
    this.nameSearch = options.nameSearch;
    this.groupByLoader = options.groupBy;
    this.multipleGroupBy = options.groupBy?.multiple === true;
    this.maxGroupLevels = options.groupBy?.maxLevels ?? 6;
    if (this.fields.get("active")?.kind === "boolean") {
      this.filters.push(
        { id: "filter_active", description: "Active", domain: [["active", "=", true]], isActive: false, groupNumber: 1 },
        { id: "filter_archived", description: "Archived", domain: [["active", "=", false]], isActive: false, groupNumber: 1 }
      );
    }
    let groupNumber = 2;
    for (const field of this.fields.values()) {
      if (field.kind !== "date" && field.kind !== "datetime") continue;
      this.filters.push({ id: `filter_date_${field.name}`, fieldName: field.name, description: field.label,
        domain: [], isActive: false, groupNumber: groupNumber++, type: "dateFilter",
        dateOptions: [ ["month", "This Month"], ["quarter", "This Quarter"], ["year", "This Year"], ["last_year", "Last Year"] ].map(([period, label]) => ({
          id: `date_${field.name}_${period}`, label: label!, domain: [], isActive: false
        }))
      });
    }
    this.refreshDates();
  }

  private refreshDates(): void {
    const now = new Date(), year = now.getFullYear(), month = now.getMonth(), quarter = Math.floor(month / 3) * 3;
    const bounds = [[year, month, year, month + 1], [year, quarter, year, quarter + 3], [year, 0, year + 1, 0], [year - 1, 0, year, 0]];
    for (const filter of this.filters) {
      if (!filter.dateOptions || !filter.fieldName) continue;
      const field = this.fields.get(filter.fieldName)!;
      const format = (y: number, m: number): string => {
        const date = new Date(y, m, 1);
        const day = `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-01`;
        return field.kind === "datetime" ? day + " 00:00:00" : day;
      };
      filter.dateOptions.forEach((option, index) => {
        const b = bounds[index]!;
        option.domain = [[field.name, ">=", format(b[0]!, b[1]!)], [field.name, "<", format(b[2]!, b[3]!)]];
        const facet = this.facets.find(f => f.filterId === filter.id && f.optionId === option.id);
        if (facet) facet.domain = option.domain;
      });
    }
  }

  subscribe(listener: () => void): () => void {
    this.listeners.push(listener);
    return () => { this.listeners = this.listeners.filter(l => l !== listener); };
  }
  private notify(): void {
    if (this.mutating) { this.deferredNotification = true; return; }
    for (const listener of this.listeners) listener();
  }
  getFacets(): SearchFacet[] { return structuredClone(this.facets); }
  /** Register bounded admitted definitions before URL state is loaded. */
  addDeclaredFilters(filters: readonly FilterItem[]): void {
    if (this.declaredFilters.length + filters.length > MAX_ITEMS || filters.some(f => this.filters.some(old => old.id === f.id)) || new Set(filters.map(f => f.id)).size !== filters.length) invalid("Duplicate or excessive declared filters.");
    this.declaredFilters.push(...structuredClone(filters));
    this.filters.push(...structuredClone(filters));
  }
  getFilters(): FilterItem[] { this.refreshDates(); return structuredClone(this.filters); }
  /** True when the owner groups by several fields and by date intervals (the pivot view). */
  hasGroupByIntervals(): boolean { return this.multipleGroupBy; }
  /** True when this list can be grouped (the owner supplied a catalogue loader). */
  hasGroupBy(): boolean { return this.groupByLoader !== undefined; }
  /** True once the Group By catalogue is loaded; `getGroupBys` lists the declared items only from then on. */
  groupByReady(): boolean { return this.catalogue !== undefined; }
  /** Load the Group By catalogue once; a failure is returned to the caller and the next call tries again. */
  loadGroupByCatalogue(): Promise<void> {
    const loader = this.groupByLoader;
    if (!loader) return Promise.reject(new ClientError("unsupported", "Grouped lists are not available yet."));
    this.catalogueLoad ??= loader.load().then(catalogue => {
      const names = new Set(catalogue.fields.map(field => field.name));
      if (catalogue.items.some(item => !names.has(item.fieldName))) invalid("group by catalogue");
      this.catalogue = catalogue;
    }, error => { this.catalogueLoad = undefined; throw error; });
    return this.catalogueLoad;
  }
  private activeGroupBy(): SearchFacet | undefined { return this.facets.find(f => f.type === "groupBy"); }
  /** The search view's declared group-by items, then the active custom one, each with its active state. */
  getGroupBys(): GroupByItem[] {
    const active = this.activeGroupBy(), ids = active?.groupByIds ?? [];
    const result: GroupByItem[] = (this.catalogue?.items ?? []).map(item => ({ ...item, isActive: ids.includes(item.id) }));
    ids.forEach((id, index) => { if (id.startsWith("custom:")) result.push({ id, description: active!.values[index]!, fieldName: active!.groupByFields![index]!, isActive: true, custom: true }); });
    return result;
  }
  /** The fields "Add Custom Group" offers: the ones the server reports as groupable. */
  getGroupableFields(): GroupableField[] { return structuredClone(this.catalogue?.fields ?? []); }
  /** One groupable field by name, once the catalogue is loaded. */
  getGroupableField(name: string): GroupableField | undefined {
    const found = this.catalogue?.fields.find(field => field.name === name);
    return found && structuredClone(found);
  }
  /** Declared group-by items the menu leaves out, each with its reason. */
  getSkippedGroupBys(): { description: string; reason: string }[] { return structuredClone(this.catalogue?.skipped ?? []); }
  getFavorites(): FavoriteItem[] { return []; }
  clearListState(): void { this.listState = undefined; }
  getListState(): ListGroupState | undefined { return this.listState && structuredClone(this.listState); }
  setListState(state: ListGroupState): void { this.listState = decodeListState(state); }
  getGroupAggregates() { return structuredClone(this.catalogue?.aggregates ?? []); }
  /** The groupings, outermost first: `field`, or `field:interval` for a date grouped by an interval. One at most unless the owner groups by several. */
  getGroupBy(): string[] { return [...this.activeGroupBy()?.groupByFields ?? []]; }
  private activateGroupBy(id: string, spec: string, label: string): void {
    const active = this.activeGroupBy();
    if (active?.groupByFields?.includes(spec)) return;
    this.listState = undefined;
    if (!active) {
      this.checkCapacity();
      this.facets.push({ id: GROUP_BY_FACET, type: "groupBy", title: "Group By", values: [label], separator: ">", groupByField: spec, groupById: id, groupByFields: [spec], groupByIds: [id] });
    } else {
      if (!this.multipleGroupBy) throw new ClientError("unsupported", "Grouping by more than one field (nested groups) is not available yet. Remove the current grouping first.");
      if (active.groupByIds!.length >= this.maxGroupLevels) invalid(`group by: at most ${this.maxGroupLevels} fields`);
      active.groupByIds!.push(id); active.groupByFields!.push(spec); active.values.push(label);
    }
    this.notify();
  }
  /** Turn a declared item on or off. */
  toggleGroupBy(id: string): void {
    if (!this.groupByLoader) return groupingUnavailable();
    this.commitMutation(() => {
      const active = this.activeGroupBy(), at = active?.groupByIds?.indexOf(id) ?? -1;
      if (active && at >= 0) {
        this.listState = undefined;
        if (active.groupByIds!.length === 1) { this.removeFacet(active.id); return; }
        for (const list of [active.groupByIds!, active.groupByFields!, active.values]) list.splice(at, 1);
        active.groupById = active.groupByIds![0]; active.groupByField = active.groupByFields![0];
        this.notify(); return;
      }
      const item = this.catalogue?.items.find(candidate => candidate.id === id);
      if (!item) return invalid("group by item");
      this.activateGroupBy(item.id, item.interval ? `${item.fieldName}:${item.interval}` : item.fieldName, item.description);
    });
  }
  /** Group by one groupable field chosen from the field list; a date field may carry its interval as `field:interval`. */
  addCustomGroupBy(spec: string): void {
    if (!this.groupByLoader) return groupingUnavailable();
    this.commitMutation(() => {
      const [fieldName, interval, ...rest] = spec.split(":");
      const field = this.catalogue?.fields.find(candidate => candidate.name === fieldName);
      const intervalLabel = GROUP_INTERVALS.find(([key]) => key === interval)?.[1];
      const dated = (field?.kind as string) === "date" || (field?.kind as string) === "datetime";
      if (!field || rest.length || (this.groupByLoader?.persist && dated && interval === undefined) || (interval !== undefined && !(this.multipleGroupBy && dated && intervalLabel))) return invalid("group by field");
      if (this.activeGroupBy()?.groupByIds?.includes(`custom:${spec}`)) return;
      this.activateGroupBy(`custom:${spec}`, spec, intervalLabel ? `${field.label}: ${intervalLabel}` : field.label);
    });
  }
  saveFavorite(_name: string, _isDefault: boolean, _isShared: boolean): FavoriteItem { return favoritesUnavailable(); }
  toggleFavorite(_id: string): void { favoritesUnavailable(); }
  deleteFavorite(_id: string): void { favoritesUnavailable(); }

  getSearchableFields(): Field[] {
    const seen = new Set<string>();
    const pick = (list: Iterable<Field>): Field[] => [...list].filter(f => {
      if (seen.has(f.name) || !searchableKinds.includes(f.kind)) return false;
      seen.add(f.name); return true;
    });
    // The view's own fields in its order; a view that declares none searchable falls back to the model's.
    const declared = pick(this.viewFields);
    return declared.length ? declared : pick(this.fields.values());
  }
  /** One label per field for the selector, the suggestions and the chip: the view's wording wins. */
  fieldLabel(name: string): string {
    const field = this.viewFields.find(f => f.name === name) ?? this.fields.get(name);
    return field?.label || name;
  }
  /**
    A
   *  selection offers its options whose label matches; a many2one row expands into the matching
   *  records (`loadChildren`); relational rows search the related records' names. 
   */
  getAutocompleteItems(searchTerm: string): AutocompleteItem[] {
    if (!searchTerm.trim()) return [];
    const value = term(searchTerm, "search value"), lower = value.toLowerCase();
    const result: AutocompleteItem[] = [];
    for (const field of this.getSearchableFields()) {
      const base = { fieldName: field.name, fieldLabel: this.fieldLabel(field.name) };
      if (field.kind === "integer") {
        if (/^-?\d+$/.test(value) && Number.isSafeInteger(Number(value))) result.push({ ...base, id: `auto_${field.name}`, label: `Search ${base.fieldLabel} for: "${value}"`, searchTerm: value, operator: "=" });
      } else if (field.kind === "selection") {
        for (const [key, label] of field.selection ?? []) {
          if (label.toLowerCase().includes(lower)) result.push({ ...base, id: `auto_${field.name}_${key}`, label: `Search ${base.fieldLabel} for: "${label}"`, searchTerm: key, operator: "=", display: label });
        }
      } else {
        result.push({ ...base, id: `auto_${field.name}`, label: `Search ${base.fieldLabel} for: "${value}"`, searchTerm: value, operator: "ilike",
          ...(field.kind === "many2one" && this.nameSearch ? { isParent: true } : {}) });
      }
    }
    return result;
  }
  /** The records an expandable many2one row stands for, as rows that search that one record. */
  async loadChildren(item: AutocompleteItem): Promise<AutocompleteItem[]> {
    const field = this.fields.get(item.fieldName);
    if (!item.isParent || !this.nameSearch || field?.kind !== "many2one" || !field.relation) return invalid("expandable search row");
    const records = await this.nameSearch(field.relation, item.searchTerm);
    return records.map(found => ({ id: `${item.id}_${found.id}`, label: found.display_name, fieldName: field.name, fieldLabel: this.fieldLabel(field.name),
      searchTerm: String(found.id), operator: "=", display: found.display_name, isChild: true, parentId: item.id }));
  }
  /** How a facet value reads: a selection's label, a picked record's name. */
  valueLabel(fieldName: string, operator: string, value: string): string {
    const field = this.fields.get(fieldName);
    if (field?.kind === "selection") return field.selection?.find(([key]) => key === value)?.[1] ?? value;
    if (field?.kind === "many2one" && operator === "=") return this.recordNames.get(`${fieldName}:${value}`) ?? `#${value}`;
    return value;
  }

  private value(fieldName: string, operator: string, raw: unknown): string | number | boolean {
    const field = this.fields.get(fieldName);
    if (!field) throw new ClientError("validation", `Unknown field '${fieldName}'.`);
    if (!operators.includes(operator)) throw new ClientError("validation", `Unsupported operator '${operator}'.`);
    if (operator === "ilike" && !["char", "text", "many2one", "one2many", "many2many"].includes(field.kind)) return invalid(`operator for ${field.label}`);
    // A related record's name (`ilike`) searches the comodel's display names on the server.
    if (operator === "ilike" && field.kind !== "char" && field.kind !== "text") return term(raw, `value for ${field.label}`);
    if (field.kind === "boolean") {
      if (!["=", "!="].includes(operator)) return invalid(`operator for ${field.label}`);
      if (raw === true || raw === "true") return true;
      if (raw === false || raw === "false") return false;
      return invalid(`boolean value for ${field.label}`);
    }
    if (["integer", "float", "many2one"].includes(field.kind)) {
      if (typeof raw !== "number" && typeof raw !== "string" || String(raw).trim() === "" || String(raw).length > MAX_VALUE) return invalid(`number for ${field.label}`);
      const numeric = String(raw).trim();
      if (!(field.kind === "float" ? /^-?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/ : /^-?\d+$/).test(numeric)) return invalid(`number for ${field.label}`);
      const value = Number(raw);
      if (!Number.isFinite(value) || field.kind !== "float" && !Number.isSafeInteger(value) || field.kind === "many2one" && value <= 0) return invalid(`number for ${field.label}`);
      return value;
    }
    if (!["char", "text", "selection", "date", "datetime"].includes(field.kind)) {
      throw new ClientError("unsupported", `Searching ${field.kind} fields is not supported.`);
    }
    const value = term(raw, `value for ${field.label}`);
    if (field.kind === "selection" && !field.selection?.some(([key]) => key === value)) return invalid(`selection for ${field.label}`);
    if (field.kind === "date" || field.kind === "datetime") {
      const pattern = field.kind === "date" ? /^\d{4}-\d{2}-\d{2}$/ : /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/;
      const parsed = new Date(value.replace(" ", "T") + (field.kind === "datetime" ? "Z" : "T00:00:00Z"));
      if (!pattern.test(value) || !Number.isFinite(parsed.getTime()) || parsed.toISOString().slice(0, field.kind === "date" ? 10 : 19).replace("T", " ") !== value) return invalid(`date for ${field.label}`);
    }
    return value;
  }
  private fieldDomain(field: string, values: string[], operator: string): unknown[] {
    return orDomains(values.map(v => [[field, operator, this.value(field, operator, v)]]));
  }
  /** Roll back failed admission before any subscriber can observe a new domain or chip. */
  private commitMutation(change: () => void): void {
    const before = structuredClone({facets:this.facets,filters:this.filters,sequence:this.sequence,listState:this.listState});
    let notify = false;
    this.mutating = true; this.deferredNotification = false;
    try {
      change();
      // The owner folds every open group on any search change, so the address is checked without them: a filter is not refused for the groups it closes.
      const kept = this.listState; this.listState = undefined;
      try { const params = new URLSearchParams(); this.serializeToUrlParams(params); this.groupByLoader?.accept?.(params); }
      finally { this.listState = kept; }
      notify = this.deferredNotification;
    } catch (error) {
      this.facets = before.facets; this.filters = before.filters; this.sequence = before.sequence; this.listState = before.listState;
      throw error;
    } finally { this.mutating = false; this.deferredNotification = false; }
    if (notify) this.notify();
  }
  private checkCapacity(): void { if (this.facets.length >= MAX_ITEMS) invalid("search: at most 32 facets"); }
  addFieldFacet(fieldName: string, value: string, operator = defaultOperator(this.fields.get(fieldName)), display?: string): void {
    this.commitMutation(() => {
      const field = this.fields.get(fieldName);
      let trimmed = term(value, "search value");
      // A typed selection label means its key, as the autocomplete rows do.
      if (field?.kind === "selection" && !field.selection?.some(([key]) => key === trimmed)) {
        trimmed = field.selection?.find(([, label]) => label.toLowerCase() === trimmed.toLowerCase())?.[0] ?? trimmed;
      }
      if (display !== undefined && field?.kind === "many2one" && operator === "=") this.recordNames.set(`${fieldName}:${trimmed}`, display);
      this.value(fieldName, operator, trimmed);
      const existing = this.facets.find(f => f.type === "field" && f.fieldName === fieldName && f.operator === operator);
      if (existing) {
        if (existing.values.includes(trimmed)) return;
        if (existing.values.length >= MAX_ITEMS) invalid("search values: at most 32");
        const values = [...existing.values, trimmed];
        existing.domain = this.fieldDomain(fieldName, values, operator); existing.values = values;
      } else {
        this.checkCapacity();
        this.facets.push({ id: `field_${++this.sequence}`, type: "field", title: this.fieldLabel(fieldName), values: [trimmed], separator: "or", fieldName, operator, domain: this.fieldDomain(fieldName, [trimmed], operator) });
      }
      this.notify();
    });
  }
  editFieldFacet(id: string, value: string): void {
    this.commitMutation(() => {
      const facet = this.facets.find(f => f.id === id);
      if (!facet || facet.type !== "field" || !facet.fieldName || !facet.operator) return invalid("field facet");
      if (!value.trim()) { this.removeFacet(id); return; }
      const values = [term(value, "search value")];
      facet.domain = this.fieldDomain(facet.fieldName, values, facet.operator); facet.values = values; this.notify();
    });
  }
  removeFacet(id: string): void {
    const facet = this.facets.find(f => f.id === id);
    if (!facet) return;
    this.facets = this.facets.filter(f => f.id !== id);
    const filter = this.filters.find(f => f.id === facet.filterId);
    if (filter) {
      if (facet.optionId) {
        const option = filter.dateOptions?.find(o => o.id === facet.optionId);
        if (option) option.isActive = false;
        filter.isActive = !!filter.dateOptions?.some(o => o.isActive);
      } else filter.isActive = false;
      if (filter.custom) this.filters = this.filters.filter(f => f !== filter);
    }
    this.notify();
  }
  removeLastFacet(): SearchFacet | null {
    const last = this.facets.at(-1);
    if (!last) return null;
    this.removeFacet(last.id); return structuredClone(last);
  }
  toggleFilter(id: string): void {
    this.commitMutation(() => {
      const filter = this.filters.find(f => f.id === id);
      if (!filter || filter.dateOptions) return invalid("filter");
      if (filter.isActive) { this.removeFacet(id); return; }
      this.checkCapacity(); filter.isActive = true;
      this.facets.push({ id, filterId: id, type: "filter", title: filter.description, values: [filter.description], separator: "or", domain: filter.domain });
      this.notify();
    });
  }
  toggleDateOption(filterId: string, optionId: string): void {
    this.commitMutation(() => {
      this.refreshDates();
      const filter = this.filters.find(f => f.id === filterId), option = filter?.dateOptions?.find(o => o.id === optionId);
      if (!filter || !option) return invalid("date option");
      const id = `${filterId}:${optionId}`;
      if (option.isActive) { this.removeFacet(id); return; }
      this.checkCapacity(); option.isActive = true; filter.isActive = true;
      this.facets.push({ id, filterId, optionId, type: "filter", title: `${filter.description}: ${option.label}`, values: [option.label], separator: "or", domain: option.domain });
      this.notify();
    });
  }
  addCustomFilter(fieldName: string, operator: string, raw: unknown): void {
    this.commitMutation(() => {
      const value = this.value(fieldName, operator, raw);
      this.checkCapacity();
      const id = `custom_${++this.sequence}`, description = `${this.fields.get(fieldName)!.label} ${operator} "${String(value)}"`, domain = [[fieldName, operator, value]];
      this.filters.push({ id, description, domain, isActive: true, custom: { field: fieldName, operator, value } });
      this.facets.push({ id, filterId: id, type: "filter", title: description, values: [description], separator: "or", domain }); this.notify();
    });
  }
  getDomain(): unknown[] {
    this.refreshDates();
    const domain = structuredClone(this.initialDomain), groups = new Map<number | string, unknown[][]>();
    for (const facet of this.facets) {
      if (!facet.domain?.length) continue;
      const filter = this.filters.find(f => f.id === facet.filterId);
      if (!filter) domain.push(...facet.domain);
      else {
        const key = filter.groupNumber ?? filter.id;
        const group = groups.get(key) ?? []; group.push(facet.domain); groups.set(key, group);
      }
    }
    for (const group of groups.values()) domain.push(...orDomains(group));
    return structuredClone(domain);
  }
  clearAll(): void {
    this.listState = undefined;
    this.facets = []; this.filters = this.filters.filter(f => !f.custom);
    for (const filter of this.filters) { filter.isActive = false; for (const option of filter.dateOptions ?? []) option.isActive = false; }
    this.notify();
  }

  serializeToUrlParams(params: URLSearchParams): void {
    const fields = this.facets.filter(f => f.type === "field").map(f => ({ field: f.fieldName, values: f.values, operator: f.operator }));
    const filters = this.facets.filter(f => f.type === "filter").map(f => {
      const filter = this.filters.find(item => item.id === f.filterId)!;
      return filter.custom ?? (f.optionId ? `${f.filterId}:${f.optionId}` : f.filterId);
    });
    const entries = [["search_facets", fields], ["search_filters", filters]] as const;
    const encoded = entries.map(([key, value]) => [key, value.length ? JSON.stringify(value) : ""] as const);
    for (const [key, value] of encoded) if (value.length > MAX_PARAM) invalid(`${key}: exceeds 2048 characters`);
    for (const [key, value] of encoded) { if (value) params.set(key, value); else params.delete(key); }
    if (this.groupByLoader?.persist && this.getGroupBy().length) {
      const state = this.listState ? { ...this.listState, by: this.getGroupBy() } : this.getGroupBy();
      const value = JSON.stringify(state);
      if (value.length > MAX_PARAM) invalid("search_groupby: exceeds 2048 characters");
      params.set("search_groupby", value);
    } else params.delete("search_groupby");
    params.delete("search");
  }

  /** Validate everything in isolation before replacing state or notifying subscribers. */
  loadFromUrlParams(params: URLSearchParams): boolean {
    const keys = ["search_facets", "search_filters", "search_groupby"];
    if (!keys.some(key => params.has(key))) return false;
    const staging = new SearchModel({ model: this.model, fields: this.fields, viewFields: this.viewFields, initialDomain: this.initialDomain, groupBy: this.groupByLoader, filters: this.declaredFilters });
    staging.catalogue = this.catalogue;
    for (const key of keys) {
      const raw = params.get(key);
      if (raw === null) continue;
      if (params.getAll(key).length !== 1 || !raw || raw.length > MAX_PARAM) invalid(key);
      let data: unknown;
      try { data = JSON.parse(raw); } catch { return invalid(key); }
      try {
        if (key === "search_groupby") {
          if (!this.groupByLoader?.persist) { if (!Array.isArray(data) || data.length) groupingFromAddress(); continue; }
          const state = decodeListState(data);
          for (const spec of state.by) {
            const declared = this.catalogue?.items.find(item => (item.interval ? `${item.fieldName}:${item.interval}` : item.fieldName) === spec);
            if (declared) staging.toggleGroupBy(declared.id); else staging.addCustomGroupBy(spec);
          }
          staging.listState = state; continue;
        }
        const values = items(data, key);
        const seen = new Set<string>();
        for (const item of values) {
          const identity = JSON.stringify(item);
          if (seen.has(identity)) invalid(key); seen.add(identity);
          if (key === "search_facets") {
            const entry = record(item, ["field", "values", "operator"], key);
            const field = term(entry["field"], key), operator = entry["operator"] === undefined ? "ilike" : term(entry["operator"], key);
            const terms = items(entry["values"], key);
            if (!terms.length || new Set(terms).size !== terms.length) invalid(key);
            for (const value of terms) staging.addFieldFacet(field, term(value, key), operator);
          } else if (typeof item === "string") {
            const parts = item.split(":");
            if (parts.length === 1) staging.toggleFilter(item);
            else if (parts.length === 2) staging.toggleDateOption(parts[0]!, parts[1]!);
            else invalid(key);
          } else {
            const entry = record(item, ["field", "operator", "value"], key);
            staging.addCustomFilter(term(entry["field"], key), term(entry["operator"], key), entry["value"]);
          }
        }
      } catch (error) {
        if (error instanceof ClientError && error.kind === "unsupported") throw error;
        if (error instanceof ClientError) return invalid(key);
        throw error;
      }
    }
    this.facets = staging.facets; this.filters = staging.filters; this.sequence = staging.sequence; this.listState = staging.listState;
    this.notify(); return true;
  }
}

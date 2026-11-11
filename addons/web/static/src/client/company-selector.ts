/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

export interface CompanyItem {
  id: number;
  name: string;
  parentId: number | null;
  sequence?: number;
}

export interface VisibleCompany {
  company: CompanyItem;
  level: number;
}

/**
 * Company tree hierarchy and active-company selection model.
 *
 * Implements multi-select, parent-branch cascade selection and deselection,
 * and single-company mode recognition.
 */
export class CompanySelector {
  private readonly companiesMap = new Map<number, CompanyItem>();
  private readonly allowedSet: Set<number>;
  private readonly childrenMap = new Map<number, number[]>();
  private readonly roots: number[] = [];

  public selectedCompanyIds: number[];

  constructor(
    public readonly companies: CompanyItem[],
    public readonly activeCompanyIds: number[],
    public readonly allowedCompanyIds: number[]
  ) {
    this.allowedSet = new Set(allowedCompanyIds);
    for (const c of companies) {
      this.companiesMap.set(c.id, c);
      this.childrenMap.set(c.id, []);
    }

    for (const c of companies) {
      if (c.parentId !== null && this.companiesMap.has(c.parentId)) {
        this.childrenMap.get(c.parentId)!.push(c.id);
      } else {
        this.roots.push(c.id);
      }
    }

    const sorter = (aId: number, bId: number) => {
      const a = this.companiesMap.get(aId)!;
      const b = this.companiesMap.get(bId)!;
      const seqA = a.sequence ?? 0;
      const seqB = b.sequence ?? 0;
      if (seqA !== seqB) return seqA - seqB;
      const nameCmp = a.name.localeCompare(b.name);
      if (nameCmp !== 0) return nameCmp;
      return a.id - b.id;
    };

    this.roots.sort(sorter);
    for (const children of this.childrenMap.values()) {
      children.sort(sorter);
    }

    this.selectedCompanyIds = [...activeCompanyIds];
  }

  get currentCompanyId(): number {
    return this.activeCompanyIds[0] ?? this.companies[0]?.id ?? 1;
  }

  get selectedCurrentCompanyId(): number {
    return this.selectedCompanyIds[0] ?? this.currentCompanyId;
  }

  getCompany(id: number): CompanyItem | undefined {
    return this.companiesMap.get(id);
  }

  getBranches(companyId: number): number[] {
    return this.childrenMap.get(companyId) ?? [];
  }

  isCompanyAllowed(companyId: number): boolean {
    return this.allowedSet.has(companyId);
  }

  isCompanySelected(companyId: number): boolean {
    return this.selectedCompanyIds.includes(companyId);
  }

  get hasSelectionChanged(): boolean {
    if (this.selectedCompanyIds.length !== this.activeCompanyIds.length) {
      return true;
    }
    for (let i = 0; i < this.selectedCompanyIds.length; i++) {
      if (this.selectedCompanyIds[i] !== this.activeCompanyIds[i]) {
        return true;
      }
    }
    return false;
  }

  reset(): void {
    this.selectedCompanyIds = [...this.activeCompanyIds];
  }

  selectCompany(companyId: number, unshift = false): void {
    if (this.isCompanyAllowed(companyId)) {
      if (!this.selectedCompanyIds.includes(companyId)) {
        if (unshift) {
          this.selectedCompanyIds.unshift(companyId);
        } else {
          this.selectedCompanyIds.push(companyId);
        }
      } else if (unshift) {
        const index = this.selectedCompanyIds.indexOf(companyId);
        if (index > 0) {
          this.selectedCompanyIds.splice(index, 1);
          this.selectedCompanyIds.unshift(companyId);
        }
      }
    }
    // Selecting a parent selects its branches
    for (const childId of this.getBranches(companyId)) {
      this.selectCompany(childId, false);
    }
  }

  deselectCompany(companyId: number): void {
    const idx = this.selectedCompanyIds.indexOf(companyId);
    if (idx !== -1) {
      this.selectedCompanyIds.splice(idx, 1);
    }
    // Deselect cascades to all branches
    for (const childId of this.getBranches(companyId)) {
      this.deselectCompany(childId);
    }
  }

  switchCompany(mode: "toggle" | "loginto", companyId: number): void {
    if (mode === "toggle") {
      if (this.selectedCompanyIds.includes(companyId)) {
        this.deselectCompany(companyId);
      } else {
        this.selectCompany(companyId, false);
      }
    } else if (mode === "loginto") {
      if (this.isSingleCompanyMode()) {
        this.selectedCompanyIds = [];
      }
      this.selectCompany(companyId, true);
    }
  }

  /**
   * Returns true when:
   * 1) Exactly 1 company is selected, OR
   * 2) All selected companies belong to a single root company, and ALL descendants
   *    (branches and sub-branches) of that root company are active.
   */
  isSingleCompanyMode(): boolean {
    if (this.selectedCompanyIds.length <= 1) {
      return true;
    }

    const getActive = (id: number | null): CompanyItem | null => {
      if (id !== null && this.selectedCompanyIds.includes(id)) {
        return this.companiesMap.get(id) ?? null;
      }
      return null;
    };

    let rootCompany: CompanyItem | null = null;
    for (const companyId of this.selectedCompanyIds) {
      let company = getActive(companyId);
      if (!company) continue;

      // Find the root active parent
      while (company.parentId !== null && getActive(company.parentId)) {
        company = getActive(company.parentId)!;
      }

      if (rootCompany === null) {
        rootCompany = company;
      } else if (rootCompany.id !== company.id) {
        return false;
      }
    }

    // Check that all children and sub-children of rootCompany are active
    if (rootCompany) {
      const queue = [...this.getBranches(rootCompany.id)];
      while (queue.length > 0) {
        const branchId = queue.pop()!;
        const branch = getActive(branchId);
        if (!branch) {
          return false;
        }
        queue.push(...this.getBranches(branch.id));
      }
    }

    return true;
  }

  selectAll(visibleCompanyIds: number[]): void {
    const hasAnySelected = visibleCompanyIds.some(id => this.selectedCompanyIds.includes(id));
    if (hasAnySelected) {
      for (const id of visibleCompanyIds) {
        this.deselectCompany(id);
      }
    } else {
      for (const id of visibleCompanyIds) {
        this.selectCompany(id, false);
      }
    }
  }

  computeVisibleCompanies(filter = ""): VisibleCompany[] {
    const normalizedFilter = filter.toLowerCase().replace(/\s+/g, "");
    const matches = (name: string) => {
      if (!normalizedFilter) return true;
      return name.toLowerCase().replace(/\s+/g, "").includes(normalizedFilter);
    };

    const result: VisibleCompany[] = [];
    const addCompany = (companyId: number, level: number) => {
      const company = this.companiesMap.get(companyId);
      if (!company) return;

      if (matches(company.name)) {
        result.push({ company, level });
      }

      for (const childId of this.getBranches(company.id)) {
        addCompany(childId, level + 1);
      }
    };

    for (const rootId of this.roots) {
      addCompany(rootId, 0);
    }

    return result;
  }
}

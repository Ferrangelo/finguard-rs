# Finguard — Build Plan

A premium dark-mode personal finance dashboard with 4 tabs (Expenses, Cashflow, NetWorth, Categories), a global Year/Month context, and a mockable API layer ready to swap for a real backend.

## Design system

- **Theme**: deep zinc/slate background (`oklch(0.16 0.02 260)` base, `oklch(0.20 0.025 260)` surfaces), glassmorphic cards (`backdrop-blur-xl`, translucent borders), subtle inner highlights.
- **Accent**: harmonic gradient — teal → violet (`oklch(0.78 0.14 200)` → `oklch(0.70 0.18 295)`), plus emerald (positive) and rose (negative) semantic tokens.
- **Typography**: Inter via `<link>` in `__root.tsx`; tight tracking on headings.
- **Animations**: `fade-in`, `scale-in`, hover lift on cards, tab transitions, shimmer on loading skeletons.
- **Charts**: Recharts everywhere — custom dark tooltip with formatted currency, toggleable legends, responsive containers.

## Global shell

- **Header**: Finguard wordmark + shield/gradient icon, Year dropdown (derived from data), Month dropdown (Jan..Dec), live status pill (idle / success / error / loading) reflecting last API action.
- **Tabs**: TanStack Router routes — `/` (redirects to `/expenses`), `/expenses`, `/cashflow`, `/networth`, `/categories`. Sub-tabs inside Expenses and NetWorth use in-page tab state.
- **Global context**: `AppContext` providing `{ year, month, setYear, setMonth, status, setStatus, refCurrency: "EUR" }`.

## Tab 1 — Expenses (4 sub-tabs)

1. **Detailed**: filter bar (name, category, min/max), data table (Date, Name, Amount, Currency, Ref€, Primary, Secondary), inline edit, delete-with-confirm. Add/Edit card with Name (auto-fills categories from mappings), Day-of-month, Amount (math-expression evaluator: `"10+5.5"` → 15.5), Currency (default EUR), Primary + Secondary auto-suggest combobox.
2. **Summary**: Primary/Secondary toggle. Monthly totals table + pie. Year-cumulative wide table (Jan..Dec) + year pie. Comparison bar chart (pick up to 3 months). Category trend lines (pick up to 3 categories across months).
3. **Recurring**: table of templates + add/delete + "Apply Recurring to Month" button (skips duplicates by name+day).
4. **Mappings**: table of name → {primary, secondary} rules + add/delete.

## Tab 2 — Cashflow

Wide grid: rows = Salary, Interests Bank account, Dividendi e Cedole, Other (editable) + derived Income, Spending (from expenses primary-cat total per month), Saving, Saving %. Charts: grouped bars (Income/Spending/Saving per month) + income distribution pie.

## Tab 3 — NetWorth (3 sub-tabs)

1. **Investments**: wide table (Asset, Category, Link, Jan..Dec), inner toggle Holdings / Prices / Value (value = qty × price, read-only). Add/edit/delete asset.
2. **Liquidity & Credits/Debts**: two stacked editable tables with Jan..Dec columns and bold totals.
3. **Total NetWorth**: computed grid (investment category sums, liquidity total, credit/debt total, total, Δ, Δ%); January Δ uses prior-year December. Allocation pie for active month + stacked area evolution with bold total line.

## Tab 4 — Categories

Split layout (Primary | Secondary), add-form per side, table with name + cumulative ref-currency total across all years; delete disabled when total > 0 with "has existing expenses" notice.

## Technical details

- **Routing**: TanStack Router file-based; routes `expenses.tsx`, `cashflow.tsx`, `networth.tsx`, `categories.tsx`, plus layout in `__root.tsx`.
- **State**: React Context + `useReducer` per domain, persisted to `localStorage` through the api client.
- **API client** (`src/services/api.ts`): all methods async returning Promises (`getExpenses(year, month)`, `upsertExpense`, `deleteExpense`, `getRecurring`, `applyRecurring(year, month)`, `getMappings`, `getCashflow(year)`, `saveCashflowCell`, `getInvestments(year)`, `saveInvestmentCell`, `getLiquidity`, `getCreditsDebts`, `getCategories`, `getCategoryTotals`, etc.). Backed by a `localStorageRepo` so swapping for fetch/Tauri = changing one module.
- **Currency conversion**: simple FX table in api (EUR=1, USD=0.92, GBP=1.17…); `refAmount = amount * fx[currency]`.
- **Math eval**: tiny safe expression evaluator (regex-validated chars `[0-9+\-*/.() ]`, then `Function`).
- **Charts**: Recharts (`ResponsiveContainer`, `PieChart`, `BarChart`, `LineChart`, `AreaChart`) with shared `<DarkTooltip />`.
- **Seed data**: a year's worth of realistic mock expenses, recurring templates, mappings, investments, liquidity rows so the app feels alive on first open.

## File layout

```text
src/
  routes/
    __root.tsx            (Inter font, header, status, nav)
    index.tsx             (redirect to /expenses)
    expenses.tsx
    cashflow.tsx
    networth.tsx
    categories.tsx
  context/AppContext.tsx
  services/
    api.ts                (public async interface)
    storage.ts            (localStorage repo + seed)
    fx.ts
    mathEval.ts
  components/
    Header.tsx, StatusPill.tsx, GlassCard.tsx, DarkTooltip.tsx,
    CurrencyInput.tsx, Combobox.tsx, ConfirmButton.tsx, SubTabs.tsx
    expenses/{DetailedTab,SummaryTab,RecurringTab,MappingsTab,ExpenseForm}.tsx
    cashflow/CashflowGrid.tsx
    networth/{InvestmentsTab,LiquidityTab,TotalTab}.tsx
    categories/CategoriesSplit.tsx
  styles.css              (full dark token set, gradient utilities)
```

## Out of scope (for this initial build)

- Real backend / auth (Cloud not enabled — easy follow-up).
- Multi-currency live FX (uses a static rates table).
- Light theme toggle.

Ready to build on approval.
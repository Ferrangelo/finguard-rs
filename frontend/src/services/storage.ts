import type {
  Categories,
  CreditDebtRow,
  Expense,
  InvestmentAsset,
  LiquidityRow,
  MappingRule,
  RecurringTemplate,
} from "./types";

const KEY = "finguard.v1";

export interface DBShape {
  expenses: Expense[];
  recurring: RecurringTemplate[];
  mappings: MappingRule[];
  categories: Categories;
  cashflowIncome: Record<number, Record<number, Record<string, number>>>;
  investments: InvestmentAsset[];
  liquidity: LiquidityRow[];
  creditsDebts: CreditDebtRow[];
}

const isBrowser = typeof window !== "undefined";
let memoryDB: DBShape | null = null;

function seed(): DBShape {
  const year = new Date().getFullYear();
  const prevYear = year - 1;
  const uid = () => Math.random().toString(36).slice(2, 10);

  const primary = [
    "Groceries","Housing","Transport","Dining","Entertainment",
    "Health","Utilities","Shopping","Travel","Other",
  ];
  const secondary = [
    "Supermarket","Rent","Fuel","Restaurant","Streaming",
    "Pharmacy","Electricity","Clothes","Flights","Other",
  ];

  const expenses: Expense[] = [];
  const sample: Array<[string, number, string, string]> = [
    ["Lidl", 48.2, "Groceries", "Supermarket"],
    ["Carrefour", 73.5, "Groceries", "Supermarket"],
    ["Rent", 1200, "Housing", "Rent"],
    ["Shell", 62.1, "Transport", "Fuel"],
    ["Trattoria Roma", 42, "Dining", "Restaurant"],
    ["Netflix", 14.99, "Entertainment", "Streaming"],
    ["Spotify", 9.99, "Entertainment", "Streaming"],
    ["Pharmacy", 18.4, "Health", "Pharmacy"],
    ["ENEL", 86.3, "Utilities", "Electricity"],
    ["Zara", 79, "Shopping", "Clothes"],
    ["Ryanair", 132, "Travel", "Flights"],
    ["Amazon", 34.5, "Shopping", "Other"],
  ];
  for (const y of [prevYear, year]) {
    for (let m = 1; m <= 12; m++) {
      if (y === year && m > new Date().getMonth() + 1) continue;
      for (const [name, amt, p, s] of sample) {
        const variance = 0.85 + Math.random() * 0.3;
        expenses.push({
          id: uid(), year: y, month: m,
          day: 1 + Math.floor(Math.random() * 27),
          name, amount: Math.round(amt * variance * 100) / 100,
          currency: "EUR", primary: p, secondary: s,
        });
      }
    }
  }

  const recurring: RecurringTemplate[] = [
    { id: uid(), name: "Rent", day: 1, amount: 1200, currency: "EUR", primary: "Housing", secondary: "Rent" },
    { id: uid(), name: "Netflix", day: 5, amount: 14.99, currency: "EUR", primary: "Entertainment", secondary: "Streaming" },
    { id: uid(), name: "Spotify", day: 7, amount: 9.99, currency: "EUR", primary: "Entertainment", secondary: "Streaming" },
    { id: uid(), name: "Gym", day: 10, amount: 39, currency: "EUR", primary: "Health", secondary: "Other" },
  ];

  const mappings: MappingRule[] = [
    { id: uid(), match: "lidl", primary: "Groceries", secondary: "Supermarket" },
    { id: uid(), match: "carrefour", primary: "Groceries", secondary: "Supermarket" },
    { id: uid(), match: "shell", primary: "Transport", secondary: "Fuel" },
    { id: uid(), match: "netflix", primary: "Entertainment", secondary: "Streaming" },
    { id: uid(), match: "enel", primary: "Utilities", secondary: "Electricity" },
    { id: uid(), match: "amazon", primary: "Shopping", secondary: "Other" },
  ];

  const cashflowIncome: DBShape["cashflowIncome"] = {};
  for (const y of [prevYear, year]) {
    cashflowIncome[y] = {};
    for (let m = 1; m <= 12; m++) {
      cashflowIncome[y][m] = {
        Salary: 3200,
        "Interests Bank account": 12,
        "Dividendi e Cedole": m % 3 === 0 ? 180 : 0,
        Other: 0,
      };
    }
  }

  const mkMonthly = <T,>(make: (m: number) => T) => {
    const r: Record<number, Record<number, T>> = {};
    for (const y of [prevYear, year]) {
      r[y] = {};
      for (let m = 1; m <= 12; m++) r[y][m] = make(m);
    }
    return r;
  };

  const investments: InvestmentAsset[] = [
    { id: uid(), name: "S&P 500 ETF", category: "Stocks/ETF", link: "https://www.ishares.com/",
      data: mkMonthly((m) => ({ qty: 12 + m * 0.5, price: 480 + m * 4 })) },
    { id: uid(), name: "World ETF", category: "Stocks/ETF",
      data: mkMonthly((m) => ({ qty: 30, price: 95 + m * 0.8 })) },
    { id: uid(), name: "Gold", category: "Commodities",
      data: mkMonthly((m) => ({ qty: 2, price: 1900 + m * 12 })) },
    { id: uid(), name: "EU Govt Bonds", category: "Bonds",
      data: mkMonthly((m) => ({ qty: 40, price: 100 + m * 0.2 })) },
  ];

  const liquidity: LiquidityRow[] = [
    { id: uid(), name: "Main Bank Account", category: "Bank/Broker account", currency: "EUR",
      data: mkMonthly((m) => 5200 + m * 80) },
    { id: uid(), name: "Savings", category: "Bank/Broker account", currency: "EUR",
      data: mkMonthly((m) => 12000 + m * 200) },
    { id: uid(), name: "Wallet", category: "Cash", currency: "EUR",
      data: mkMonthly(() => 150) },
  ];

  const creditsDebts: CreditDebtRow[] = [
    { id: uid(), name: "Friend loan", currency: "EUR", data: mkMonthly(() => 300) },
    { id: uid(), name: "Credit card", currency: "EUR", data: mkMonthly(() => -420) },
  ];

  return { expenses, recurring, mappings,
    categories: { primary, secondary },
    cashflowIncome, investments, liquidity, creditsDebts };
}

export function readDB(): DBShape {
  if (memoryDB) return memoryDB;
  if (isBrowser) {
    try {
      const raw = window.localStorage.getItem(KEY);
      if (raw) { memoryDB = JSON.parse(raw) as DBShape; return memoryDB; }
    } catch { /* ignore */ }
  }
  memoryDB = seed();
  writeDB(memoryDB);
  return memoryDB;
}

export function writeDB(db: DBShape): void {
  memoryDB = db;
  if (isBrowser) {
    try { window.localStorage.setItem(KEY, JSON.stringify(db)); } catch { /* ignore */ }
  }
}

export function resetDB(): void {
  memoryDB = null;
  if (isBrowser) window.localStorage.removeItem(KEY);
}
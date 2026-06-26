"use client";

import { createContext, useContext, useEffect, useState, type ReactNode } from "react";

type Theme = "arctic" | "midnight" | "dusk" | "ember" | "forest" | "pitch" | "original";

interface ThemeContextType {
  theme: Theme;
  setTheme: (theme: Theme) => void;
  availableThemes: Theme[];
}

const ThemeContext = createContext<ThemeContextType | undefined>(undefined);

const AVAILABLE_THEMES: Theme[] = [
  "arctic",
  "midnight",
  "dusk",
  "ember",
  "forest",
  "pitch",
  "original",
];
const DEFAULT_THEME: Theme = "midnight";
const THEME_STORAGE_KEY = "finguard-theme";

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [theme, setThemeState] = useState<Theme>(DEFAULT_THEME);

  // Load theme from localStorage on mount (runs only on the client)
  useEffect(() => {
    const storedTheme = localStorage.getItem(THEME_STORAGE_KEY) as Theme | null;
    if (storedTheme && AVAILABLE_THEMES.includes(storedTheme)) {
      setThemeState(storedTheme);
      applyTheme(storedTheme);
    } else {
      applyTheme(DEFAULT_THEME);
    }
  }, []);

  const setTheme = (newTheme: Theme) => {
    if (AVAILABLE_THEMES.includes(newTheme)) {
      setThemeState(newTheme);
      localStorage.setItem(THEME_STORAGE_KEY, newTheme);
      applyTheme(newTheme);
    }
  };

  // Always render the Provider so consumers have context during SSR too
  return (
    <ThemeContext.Provider value={{ theme, setTheme, availableThemes: AVAILABLE_THEMES }}>
      {children}
    </ThemeContext.Provider>
  );
}

export function useTheme() {
  const context = useContext(ThemeContext);
  if (!context) {
    throw new Error("useTheme must be used within ThemeProvider");
  }
  return context;
}

function applyTheme(theme: Theme) {
  // Remove previous theme stylesheets
  const previousLink = document.querySelector("link[data-theme]");
  if (previousLink) {
    previousLink.remove();
  }

  // Create and append the new theme stylesheet
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = `/src/styles/${theme}.css`;
  link.dataset.theme = theme;
  document.head.appendChild(link);
}

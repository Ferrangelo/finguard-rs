"use client";

// Multi-theme system: see frontend/THEME_SYSTEM.md for the full design.
// Each theme is a standalone CSS file under src/styles/ (Tailwind `@theme`
// variables); switching themes swaps which stylesheet `<link>` is present
// in `<head>` rather than toggling a class, so unrelated component code
// never needs to branch on the active theme. The chosen theme persists in
// localStorage under `THEME_STORAGE_KEY` and is re-applied on every mount.
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

/**
 * Provides the active theme and `setTheme` to descendants. Renders its
 * provider unconditionally (including during SSR, before the client-only
 * localStorage read in the effect below runs) so every consumer always has
 * a context value, starting at `DEFAULT_THEME` until the effect applies
 * the stored theme, if any.
 */
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

/** Reads the current theme, `setTheme`, and the list of available themes. Throws if called outside a `ThemeProvider`. */
export function useTheme() {
  const context = useContext(ThemeContext);
  if (!context) {
    throw new Error("useTheme must be used within ThemeProvider");
  }
  return context;
}

// Swaps the injected theme <link> for `theme`'s stylesheet. Removing the
// old link before appending the new one, rather than swapping `href` in
// place, avoids a brief moment with both themes' rules active.
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

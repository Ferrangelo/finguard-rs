# Theme System Documentation

## Overview

FinGuard now includes a dynamic theme switcher that allows users to change the application theme without restarting. The system is built with React Context and CSS variable management.

## Available Themes

The application includes 7 predefined themes:

1. **Arctic** - Light theme with cool, clean colors ❄️
2. **Midnight** - Dark theme with deep blues and purples (default) 🌙
3. **Dusk** - Warm sunset tones 🌅
4. **Ember** - Fiery warm orange/red theme 🔥
5. **Forest** - Green nature-inspired theme 🌲
6. **Pitch** - Ultra-dark high-contrast theme ⚡
7. **Original** - Classic color palette 🎨

## Architecture

### Components

#### `ThemeContext.tsx` (`src/context/ThemeContext.tsx`)
- **Provider:** `ThemeProvider` - Wraps the app to manage theme state
- **Hook:** `useTheme()` - Access theme state in any component
- **Features:**
  - Persistent theme storage in localStorage
  - Dynamic CSS stylesheet injection/replacement
  - Type-safe theme selection

#### `ThemeSwitcher.tsx` (`src/components/ThemeSwitcher.tsx`)
- Dropdown menu component for theme selection
- Shows current theme with checkmark
- Theme icons for visual identification
- Integrated into the Header component

### CSS Files

Each theme is defined as a complete CSS file in `src/styles/`:
- `arctic.css`
- `midnight.css`
- `dusk.css`
- `ember.css`
- `forest.css`
- `pitch.css`
- `original.css`

The CSS files define Tailwind CSS theme variables using the `@theme` directive.

## How It Works

1. **Initial Load:**
   - `ThemeProvider` checks localStorage for saved theme preference
   - Loads the stored theme or defaults to "midnight"
   - Injects the theme CSS file into `<head>`

2. **Theme Switch:**
   - User clicks ThemeSwitcher button in header
   - Selects new theme from dropdown
   - `setTheme()` is called:
     - Updates React state
     - Saves preference to localStorage
     - Removes old theme stylesheet
     - Injects new theme stylesheet

3. **Persistence:**
   - Theme preference is saved in `localStorage['finguard-theme']`
   - Preference persists across browser sessions
   - No app restart required

## Usage in Components

### Using the Theme Hook

```tsx
import { useTheme } from "@/context/ThemeContext";

export function MyComponent() {
  const { theme, setTheme, availableThemes } = useTheme();

  return (
    <div>
      <p>Current theme: {theme}</p>
      <button onClick={() => setTheme('arctic')}>
        Switch to Arctic
      </button>
    </div>
  );
}
```

## File Structure

```
src/
├── context/
│   └── ThemeContext.tsx          # Theme provider and hook
├── components/
│   ├── ThemeSwitcher.tsx         # Theme selector UI
│   └── finguard/
│       └── Header.tsx            # Contains ThemeSwitcher button
├── styles.css                     # Base styles
└── styles/
    ├── arctic.css                # Light theme
    ├── midnight.css              # Dark theme (default)
    ├── dusk.css
    ├── ember.css
    ├── forest.css
    ├── pitch.css
    └── original.css
```

## Integration Points

1. **Root Component (`src/routes/__root.tsx`)**
   - `ThemeProvider` wraps the entire app
   - Must be inside `QueryClientProvider` but outside other providers

2. **Header (`src/components/finguard/Header.tsx`)**
   - `ThemeSwitcher` component displayed in top-right
   - Between month/year selectors and StatusPill

## Customizing Themes

### Adding a New Theme

1. Create a new CSS file in `src/styles/` (e.g., `src/styles/myTheme.css`)
2. Define color variables using Tailwind CSS `@theme` syntax
3. Add theme name to `AVAILABLE_THEMES` array in `ThemeContext.tsx`
4. Add label and icon to `THEME_LABELS` in `ThemeSwitcher.tsx`

Example:
```css
/* src/styles/myTheme.css */
@import "tailwindcss" source(none);
@source "../src";
@import "tw-animate-css";

@theme inline {
  --color-background: #ffffff;
  --color-foreground: #000000;
  /* ... more variables ... */
}

:root {
  --background: #ffffff;
  --foreground: #000000;
  /* ... more CSS variables ... */
}
```

### Modifying Existing Themes

Edit the corresponding CSS file in `src/styles/` and adjust color values. Changes are immediately visible when the theme is selected.

## Technical Details

### CSS Variable Scope
- Variables are defined in `:root` selector (global scope)
- Tailwind's `@theme` directive creates utility classes from these variables
- Custom utilities (`.glass`, `.text-gradient`, etc.) reference these variables

### Dark Mode Support
- Each theme CSS file includes its own dark mode styling
- Uses `@custom-variant dark (&:is(.dark *))` for dark mode state
- HTML element has `className="dark"` applied in `__root.tsx`

### Performance Considerations
- Theme stylesheets are dynamically injected/removed
- Old stylesheet is removed before new one is added (prevents conflicts)
- localStorage saves user preference (minimal overhead)
- No re-renders required for theme switching at component level

## Troubleshooting

### Theme Not Persisting
- Check browser localStorage is enabled
- Verify `THEME_STORAGE_KEY` matches in ThemeContext
- Check browser console for errors

### Theme Not Applying
- Ensure theme CSS file exists in `src/styles/`
- Verify theme name matches filename exactly
- Check network tab to ensure CSS is loading

### Theme Flickering on Page Load
- This is expected if theme is different from default
- `ThemeProvider` applies theme on mount before render
- Consider adding a loading state if this is problematic

## Browser Compatibility

- Works in all modern browsers with localStorage support
- Gracefully degrades to default theme if localStorage unavailable
- CSS variable support required (all modern browsers)

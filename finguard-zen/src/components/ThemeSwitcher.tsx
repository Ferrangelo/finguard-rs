"use client";

import { useTheme } from "@/context/ThemeContext";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
  DropdownMenuLabel,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu";
import { Button } from "@/components/ui/button";
import { Moon, Sun, Cloud, Flame, Trees, Zap, Palette } from "lucide-react";

const THEME_LABELS: Record<string, { label: string; icon: React.ReactNode }> = {
  arctic: { label: "Arctic", icon: <Cloud className="h-4 w-4" /> },
  midnight: { label: "Midnight", icon: <Moon className="h-4 w-4" /> },
  dusk: { label: "Dusk", icon: <Sun className="h-4 w-4" /> },
  ember: { label: "Ember", icon: <Flame className="h-4 w-4" /> },
  forest: { label: "Forest", icon: <Trees className="h-4 w-4" /> },
  pitch: { label: "Pitch", icon: <Zap className="h-4 w-4" /> },
  original: { label: "Original", icon: <Palette className="h-4 w-4" /> },
};

export function ThemeSwitcher() {
  const { theme, setTheme, availableThemes } = useTheme();

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" title="Switch theme">
          {THEME_LABELS[theme]?.icon || <Palette className="h-4 w-4" />}
          <span className="sr-only">Toggle theme</span>
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuLabel>Theme</DropdownMenuLabel>
        <DropdownMenuSeparator />
        {availableThemes.map((t) => (
          <DropdownMenuItem key={t} onClick={() => setTheme(t)} className="cursor-pointer">
            {THEME_LABELS[t]?.icon && <span className="mr-2 h-4 w-4">{THEME_LABELS[t].icon}</span>}
            <span>{THEME_LABELS[t]?.label || t}</span>
            {theme === t && <span className="ml-auto text-xs">✓</span>}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

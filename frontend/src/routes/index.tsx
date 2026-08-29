import { createFileRoute, redirect } from "@tanstack/react-router";

// The root path has no page of its own; it redirects to /expenses before
// any component would render, so this route never mounts a UI.
export const Route = createFileRoute("/")({
  beforeLoad: () => {
    throw redirect({ to: "/expenses" });
  },
});

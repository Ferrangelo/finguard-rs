import { QueryClient } from "@tanstack/react-query";
import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

/**
 * Builds the TanStack Router instance used by both the server entry
 * (start.ts) and the client. Creates a fresh `QueryClient` per call and
 * passes it through as router context, which `__root.tsx` reads via
 * `Route.useRouteContext()` to supply `QueryClientProvider`. Route page
 * components in this app do not use TanStack Query themselves (see the
 * data-flow notes in routes/expenses.tsx and routes/networth.tsx); the
 * `QueryClient` exists for the router's own use and for any future query
 * usage. `defaultPreloadStaleTime: 0` disables the router's default
 * preload caching, since route loaders here do not fetch data.
 */
export const getRouter = () => {
  const queryClient = new QueryClient();

  const router = createRouter({
    routeTree,
    context: { queryClient },
    scrollRestoration: true,
    defaultPreloadStaleTime: 0,
  });

  return router;
};

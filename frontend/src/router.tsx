import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

/**
 * Builds the TanStack Router instance used by both the server entry
 * (start.ts) and the client. `defaultPreloadStaleTime: 0` disables the
 * router's default preload caching, since route loaders here do not fetch
 * data; route components refetch through the `refreshTick` counter in
 * `AppContext.tsx` instead.
 */
export const getRouter = () => {
  const router = createRouter({
    routeTree,
    scrollRestoration: true,
    defaultPreloadStaleTime: 0,
  });

  return router;
};

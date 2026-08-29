// TanStack Start configuration entry point (the framework imports this
// module to configure server request middleware). Registers
// `errorMiddleware` globally so any server-side request error not already
// carrying an HTTP `statusCode` (i.e. not an intentional framework
// redirect/not-found response) is logged and converted to the static
// fallback page, complementing the normalization server.ts does for errors
// that reach the SSR handler itself.
import { createStart, createMiddleware } from "@tanstack/react-start";

import { renderErrorPage } from "./lib/error-page";

const errorMiddleware = createMiddleware().server(async ({ next }) => {
  try {
    return await next();
  } catch (error) {
    // Errors carrying a `statusCode` are intentional framework control flow
    // (redirects, not-found responses, etc.) and must propagate unchanged.
    if (error != null && typeof error === "object" && "statusCode" in error) {
      throw error;
    }
    console.error(error);
    return new Response(renderErrorPage(), {
      status: 500,
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  }
});

export const startInstance = createStart(() => ({
  requestMiddleware: [errorMiddleware],
}));

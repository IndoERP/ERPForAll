/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";

/** App-local feedback only: never takes focus, changes layout or intercepts input. */
export function requestLoading(host: HTMLElement): { begin(): () => void; dispose(): void } {
  const status = el("div", undefined, "client-loading");
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");
  status.setAttribute("aria-atomic", "true");
  status.hidden = true;
  host.append(status);
  let count = 0, disposed = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  return {
    begin() {
      if (disposed) return () => {};
      count++;
      if (count === 1) {
        timer = setTimeout(() => {
          timer = undefined;
          if (!disposed && count > 0) {
            status.textContent = "Loading…";
            status.hidden = false;
          }
        }, 250);
      }
      let finished = false;
      return () => {
        if (finished || disposed) return;
        finished = true;
        if (--count === 0) {
          clearTimeout(timer);
          timer = undefined;
          status.hidden = true;
          status.textContent = "";
        }
      };
    },
    dispose() {
      disposed = true;
      clearTimeout(timer);
      status.remove();
    },
  };
}

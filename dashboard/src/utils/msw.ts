import { setupWorker } from "msw/browser";
import { handlers as clayHandlers } from "../api/waycast/mocks/handlers";

const allHandlers = [...clayHandlers];

export const worker = setupWorker(...allHandlers);

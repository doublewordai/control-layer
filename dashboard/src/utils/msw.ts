import { setupWorker } from "msw/browser";
import { handlers as clayHandlers } from "../api/control-layer/mocks/handlers";

const allHandlers = [...clayHandlers];

export const worker = setupWorker(...allHandlers);

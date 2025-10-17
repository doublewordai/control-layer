import { setupWorker } from "msw/browser";
import { handlers as clayHandlers } from "../api/dwctl/mocks/handlers";

const allHandlers = [...clayHandlers];

export const worker = setupWorker(...allHandlers);

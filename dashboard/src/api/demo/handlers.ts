// MSW handlers for demo API endpoints
import { http, HttpResponse } from "msw";
import requestsData from "./data/requests.json";

export const demoHandlers = [
  // Demo requests endpoint
  http.get("/api/v1/requests", ({ request }) => {
    const url = new URL(request.url);
    const limit = url.searchParams.get("limit");
    const offset = url.searchParams.get("offset");
    const userId = url.searchParams.get("user_id");
    const model = url.searchParams.get("model");

    let filteredRequests = [...requestsData];

    // Apply filters
    if (userId) {
      filteredRequests = filteredRequests.filter(
        (req) => req.metadata.email === userId,
      );
    }
    if (model) {
      filteredRequests = filteredRequests.filter((req) => req.model === model);
    }

    // Apply pagination
    const startIndex = offset ? parseInt(offset, 10) : 0;
    const endIndex = limit ? startIndex + parseInt(limit, 10) : undefined;

    const paginatedRequests = filteredRequests.slice(startIndex, endIndex);

    return HttpResponse.json(paginatedRequests);
  }),
];

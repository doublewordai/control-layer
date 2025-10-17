// Custom error class for API responses
export class ApiError extends Error {
  status: number;
  response?: Response;

  constructor(status: number, message: string, response?: Response) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.response = response;
  }
}

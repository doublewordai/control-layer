import { useContext } from "react";
import { OrganizationContext } from "./context";

export function useOrganizationContext() {
  const context = useContext(OrganizationContext);
  if (!context) {
    throw new Error(
      "useOrganizationContext must be used within OrganizationProvider",
    );
  }
  return context;
}

import { useEffect, useState } from "react";

declare global {
  interface Window {
    bootstrapContent: string;
  }
}

const BOOTSTRAP_BANNER_CLOSED_KEY = "bootstrapBannerClosed";

export const useBootstrapContent = () => {
  const [bootstrapContent, setBootstrapContent] = useState("");
  const [isClosed, setIsClosed] = useState(() => {
    return sessionStorage.getItem(BOOTSTRAP_BANNER_CLOSED_KEY) === "true";
  });

  useEffect(() => {
    setBootstrapContent(window.bootstrapContent);
  }, []);

  const close = () => {
    sessionStorage.setItem(BOOTSTRAP_BANNER_CLOSED_KEY, "true");
    setIsClosed(true);
  };

  return {
    content: bootstrapContent,
    isClosed,
    close,
  };
};

import { Light as SyntaxHighlighter } from "react-syntax-highlighter";
import python from "react-syntax-highlighter/dist/esm/languages/hljs/python";
import javascript from "react-syntax-highlighter/dist/esm/languages/hljs/javascript";
import bash from "react-syntax-highlighter/dist/esm/languages/hljs/bash";
import json from "react-syntax-highlighter/dist/esm/languages/hljs/json";
import oneDark from "react-syntax-highlighter/dist/esm/styles/hljs/atom-one-dark";
import oneLight from "react-syntax-highlighter/dist/esm/styles/hljs/atom-one-light";

// Register only the languages we need
SyntaxHighlighter.registerLanguage("python", python);
SyntaxHighlighter.registerLanguage("javascript", javascript);
SyntaxHighlighter.registerLanguage("bash", bash);
SyntaxHighlighter.registerLanguage("json", json);

interface CodeBlockProps {
  language: "python" | "javascript" | "bash" | "json";
  children: string;
  className?: string;
  variant?: "dark" | "light";
}

export function CodeBlock({
  language,
  children,
  className,
  variant = "dark",
}: CodeBlockProps) {
  return (
    <div style={{ maxWidth: "100%", overflow: "auto" }}>
      <SyntaxHighlighter
        language={language}
        style={variant === "light" ? oneLight : oneDark}
        customStyle={{
          margin: 0,
          borderRadius: 0,
          fontSize: "0.875rem",
          maxWidth: "100%",
          overflowX: "auto",
          padding: "1rem",
          ...(variant === "light" && { background: "transparent" }),
        }}
        className={className}
        wrapLongLines={false}
        PreTag="div"
      >
        {children}
      </SyntaxHighlighter>
    </div>
  );
}

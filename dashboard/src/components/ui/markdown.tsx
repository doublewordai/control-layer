import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "./code-block";

interface MarkdownProps {
  children: string;
  className?: string;
  /**
   * Compact mode uses tighter spacing for lists and paragraphs.
   * Useful for card previews or smaller containers.
   */
  compact?: boolean;
}

export function Markdown({
  children,
  className = "",
  compact = false,
}: MarkdownProps) {
  return (
    <div className={`prose prose-sm max-w-none ${className}`}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          p: ({ children, node }) => {
            // Check if this paragraph contains a code block
            // Code blocks should not be wrapped in <p> tags (invalid HTML)
            const hasCodeBlock = node?.children?.some(
              (child: any) => child.type === "element" && child.tagName === "code" && !child.properties?.inline
            );

            // If it contains a code block, return children unwrapped
            if (hasCodeBlock) {
              return <>{children}</>;
            }

            return (
              <p className={compact ? "mb-1 last:mb-0" : "mb-2 last:mb-0"}>
                {children}
              </p>
            );
          },
          ul: ({ children }) => (
            <ul
              className={`list-disc list-inside ${compact ? "mb-1 space-y-0.5" : "mb-2 space-y-1"}`}
            >
              {children}
            </ul>
          ),
          ol: ({ children }) => (
            <ol
              className={`list-decimal list-inside ${compact ? "mb-1 space-y-0.5" : "mb-2 space-y-1"}`}
            >
              {children}
            </ol>
          ),
          li: ({ children }) => {
            // ReactMarkdown wraps list item content in <p> tags by default
            // We need to extract the content to prevent unwanted line breaks
            return <li className="ml-0">{children}</li>;
          },
          code: ({ inline, className, children, ...props }: any) => {
            const match = /language-(\w+)/.exec(className || "");
            const language = match?.[1];
            const supportedLanguages = ["python", "javascript", "bash", "json"];

            return !inline &&
              language &&
              supportedLanguages.includes(language) ? (
              <CodeBlock
                language={language as "python" | "javascript" | "bash" | "json"}
              >
                {String(children).replace(/\n$/, "")}
              </CodeBlock>
            ) : !inline ? (
              <pre
                className={`bg-gray-900 text-gray-100 rounded overflow-x-auto ${compact ? "p-2 text-xs" : "p-3 text-sm"}`}
              >
                <code>{children}</code>
              </pre>
            ) : (
              <code
                className={`bg-gray-100 px-1 py-0.5 rounded ${compact ? "text-xs" : "text-sm"}`}
                {...props}
              >
                {children}
              </code>
            );
          },
          a: ({ children, href }) => (
            <a
              href={href}
              className="text-blue-600 hover:underline"
              target="_blank"
              rel="noopener noreferrer"
            >
              {children}
            </a>
          ),
        }}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
}

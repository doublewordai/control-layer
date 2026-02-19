import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "./code-block";

const SUPPORTED_LANGUAGES = ["python", "javascript", "bash", "json"] as const;
type SupportedLanguage = (typeof SUPPORTED_LANGUAGES)[number];

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
  const spacing = compact
    ? { block: "mb-1", list: "mb-1 space-y-0.5", code: "p-2 text-xs" }
    : { block: "mb-3", list: "mb-3 space-y-1", code: "p-3 text-sm" };

  return (
    <div className={`max-w-none text-sm text-gray-700 ${className}`}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          h1: ({ children }) => (
            <h1
              className={`text-xl font-semibold text-gray-900 ${spacing.block} first:mt-0 mt-6`}
            >
              {children}
            </h1>
          ),
          h2: ({ children }) => (
            <h2
              className={`text-lg font-semibold text-gray-900 ${spacing.block} first:mt-0 mt-5`}
            >
              {children}
            </h2>
          ),
          h3: ({ children }) => (
            <h3
              className={`text-base font-semibold text-gray-900 ${spacing.block} first:mt-0 mt-4`}
            >
              {children}
            </h3>
          ),
          h4: ({ children }) => (
            <h4
              className={`text-sm font-semibold text-gray-900 ${spacing.block} first:mt-0 mt-3`}
            >
              {children}
            </h4>
          ),
          p: ({ children, node }) => {
            const hasBlock = node?.children?.some(
              (child: any) =>
                child.type === "element" &&
                (child.tagName === "pre" || child.tagName === "code"),
            );
            if (hasBlock) return <>{children}</>;
            return (
              <p className={`${spacing.block} last:mb-0`}>{children}</p>
            );
          },
          ul: ({ children }) => (
            <ul className={`list-disc pl-5 ${spacing.list}`}>{children}</ul>
          ),
          ol: ({ children }) => (
            <ol className={`list-decimal pl-5 ${spacing.list}`}>
              {children}
            </ol>
          ),
          li: ({ children }) => (
            <li className="pl-1 [&>ul]:mb-0 [&>ol]:mb-0 [&>ul]:mt-1 [&>ol]:mt-1">
              {children}
            </li>
          ),
          blockquote: ({ children }) => (
            <blockquote className="border-l-2 border-gray-300 pl-4 my-3 text-gray-600 italic">
              {children}
            </blockquote>
          ),
          table: ({ children }) => (
            <div className="my-3 overflow-x-auto rounded-md border border-gray-200">
              <table className="min-w-full text-sm">{children}</table>
            </div>
          ),
          thead: ({ children }) => (
            <thead className="bg-gray-50 border-b border-gray-200">
              {children}
            </thead>
          ),
          tbody: ({ children }) => <tbody>{children}</tbody>,
          tr: ({ children }) => (
            <tr className="border-b border-gray-100 last:border-0">
              {children}
            </tr>
          ),
          th: ({ children }) => (
            <th className="px-3 py-2 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
              {children}
            </th>
          ),
          td: ({ children }) => (
            <td className="px-3 py-2 text-gray-700">{children}</td>
          ),
          hr: () => <hr className="my-4 border-gray-200" />,
          // Block code: extract language and content from the AST node
          // This handles both fenced blocks with and without a language tag
          pre: ({ node }: any) => {
            const codeNode = node?.children?.[0];
            if (codeNode?.tagName === "code") {
              const codeClassName =
                (codeNode.properties?.className as string[])?.[0] || "";
              const match = /language-(\w+)/.exec(codeClassName);
              const language = match?.[1];
              const text = (codeNode.children?.[0] as any)?.value || "";
              const content = text.replace(/\n$/, "");

              if (
                language &&
                SUPPORTED_LANGUAGES.includes(language as SupportedLanguage)
              ) {
                return (
                  <div className="my-2 rounded-md overflow-hidden">
                    <CodeBlock language={language as SupportedLanguage}>
                      {content}
                    </CodeBlock>
                  </div>
                );
              }

              return (
                <div className="my-2 rounded-md overflow-hidden">
                  <pre
                    className={`bg-gray-900 text-gray-100 overflow-x-auto ${spacing.code}`}
                  >
                    <code>{content}</code>
                  </pre>
                </div>
              );
            }

            return <pre>{(node?.children || []).map((c: any) => c.value)}</pre>;
          },
          // Inline code only — block code is handled by pre above
          code: ({ children, ...props }: any) => (
            <code
              className="bg-gray-100 text-gray-800 px-1.5 py-0.5 rounded text-[0.85em] font-mono"
              {...props}
            >
              {children}
            </code>
          ),
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
          strong: ({ children }) => (
            <strong className="font-semibold text-gray-900">{children}</strong>
          ),
        }}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
}

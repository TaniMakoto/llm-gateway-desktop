import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";

/**
 * 将模型回复的 Markdown 源文本渲染为富文本。
 *
 * - remark-gfm：表格、删除线、任务列表、自动链接。
 * - rehype-highlight：代码块语法高亮（hljs 仅负责给 token 加 class，配色由
 *   `.markdown-body` 下的样式表按主题变量统一提供，见 src/index.css）。
 *
 * react-markdown 默认对原始 HTML 转义，不使用 dangerouslySetInnerHTML，
 * 因此可安全渲染来自上游模型的文本。
 */
export function Markdown({ content }: { content: string }) {
  return (
    <div className="markdown-body">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeHighlight]}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}

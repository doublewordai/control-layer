import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";

const markdown = `Key Enhancements:

 - Visual Agent: Operates PC/mobile GUIs—recognizes elements, understands functions, invokes tools, completes tasks.

- Visual Coding Boost: Generates Draw.io/HTML/CSS/JS from images/videos.`;

const tree = unified().use(remarkParse).use(remarkGfm).parse(markdown);

console.log(JSON.stringify(tree, null, 2));

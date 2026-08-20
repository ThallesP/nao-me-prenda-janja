import "@fontsource/noto-sans/400.css";
import "@fontsource/noto-sans/500.css";
import "@fontsource/noto-sans/600.css";
import "@fontsource/noto-sans/700.css";
import "./styles.css";

import { createRoot } from "react-dom/client";

import { Activity } from "./pages/Activity";
import { Share } from "./pages/Share";

const Page = location.pathname.startsWith("/share") ? Share : Activity;

createRoot(document.getElementById("root")!).render(<Page />);

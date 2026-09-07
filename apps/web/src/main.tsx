import { StrictMode } from "react"
import { createRoot } from "react-dom/client"

import favicon from "@inseam/brand/assets/favicon.svg"

import "./index.css"
import App from "./App.tsx"

const icon = document.createElement("link")
icon.rel = "icon"
icon.type = "image/svg+xml"
icon.href = favicon
document.head.append(icon)

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>
)

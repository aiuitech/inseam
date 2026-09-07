import { ChevronDown } from "lucide-react"
import * as React from "react"

import { cn } from "../../lib/utils"

/** A native select styled to the brand; enough for a handful of options. */
function Select({
  className,
  children,
  ...props
}: React.ComponentProps<"select">) {
  return (
    <span className="relative inline-flex w-full">
      <select
        data-slot="select"
        className={cn(
          "h-8 w-full appearance-none rounded-none border border-input bg-input/30 pr-8 pl-2.5 text-xs outline-none focus-visible:border-ring focus-visible:ring-1 focus-visible:ring-ring/50 disabled:opacity-50",
          className
        )}
        {...props}
      >
        {children}
      </select>
      <ChevronDown className="pointer-events-none absolute top-1/2 right-2 size-3.5 -translate-y-1/2 text-muted-foreground" />
    </span>
  )
}

export { Select }

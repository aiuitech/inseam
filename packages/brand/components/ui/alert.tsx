import { cva, type VariantProps } from "class-variance-authority"
import * as React from "react"

import { cn } from "../../lib/utils"

const alertVariants = cva(
  "border px-4 py-3 text-xs leading-relaxed [&>svg]:mb-1 [&>svg]:size-4",
  {
    variants: {
      variant: {
        default: "border-border bg-card text-foreground",
        info: "border-primary/40 bg-primary/5 text-foreground [&>svg]:text-primary",
        warning:
          "border-warning/40 bg-warning/5 text-foreground [&>svg]:text-warning",
        destructive:
          "border-destructive/40 bg-destructive/5 text-foreground [&>svg]:text-destructive",
      },
    },
    defaultVariants: { variant: "default" },
  }
)

function Alert({
  className,
  variant,
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof alertVariants>) {
  return (
    <div
      role="alert"
      data-slot="alert"
      className={cn(alertVariants({ variant }), className)}
      {...props}
    />
  )
}

export { Alert, alertVariants }

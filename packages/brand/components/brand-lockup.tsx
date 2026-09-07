import { mergeProps } from "@base-ui/react/merge-props"
import { useRender } from "@base-ui/react/use-render"

import { cn } from "../lib/utils"

/**
 * The wordmark: the ▬●▬ mark beside lowercase `inseam`, on one line.
 *
 * Renders a plain `<span>` by default. Pass `render` to make it something
 * else, such as a router link: `<BrandLockup render={<Link to="/" />} />`.
 */
function BrandLockup({
  className,
  render,
  ...props
}: useRender.ComponentProps<"span">) {
  return useRender({
    defaultTagName: "span",
    props: mergeProps<"span">(
      {
        className: cn(
          "inline-flex items-center gap-3 font-bold tracking-tighter",
          className
        ),
        children: (
          <>
            <span className="brand-mark" aria-hidden="true">
              ▬●▬
            </span>
            <span>inseam</span>
          </>
        ),
      },
      props
    ),
    render,
    state: { slot: "brand-lockup" },
  })
}

export { BrandLockup }

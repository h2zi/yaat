import { Slot } from "radix-ui";
import { cva, type VariantProps } from "class-variance-authority";
import type * as React from "react";

import { cn } from "@/lib/utils";

const buttonVariants = cva(
  "inline-flex shrink-0 items-center justify-center gap-2 whitespace-nowrap rounded-lg text-sm font-medium transition-[color,background-color,border-color,box-shadow,transform] outline-none select-none disabled:pointer-events-none disabled:opacity-45 focus-visible:ring-[3px] focus-visible:ring-ring/30 focus-visible:border-ring active:translate-y-px [&_svg]:pointer-events-none [&_svg]:size-4",
  {
    variants: {
      variant: {
        default:
          "bg-primary text-primary-foreground shadow-sm hover:bg-primary/90",
        secondary:
          "border border-border bg-card text-foreground shadow-xs hover:bg-muted",
        outline:
          "border border-border bg-transparent text-foreground hover:bg-muted/80",
        ghost: "text-muted-foreground hover:bg-muted hover:text-foreground",
        danger: "bg-destructive/10 text-destructive hover:bg-destructive/16",
      },
      size: {
        default: "h-9 px-3.5",
        sm: "h-8 rounded-md px-2.5 text-xs",
        lg: "h-10 px-5",
        icon: "size-9 p-0",
        "icon-sm": "size-8 rounded-md p-0",
      },
    },
    defaultVariants: { variant: "default", size: "default" },
  },
);

function Button({
  className,
  variant,
  size,
  asChild = false,
  ...props
}: React.ComponentProps<"button"> &
  VariantProps<typeof buttonVariants> & { asChild?: boolean }) {
  const Comp = asChild ? Slot.Root : "button";
  return (
    <Comp
      data-slot="button"
      className={cn(buttonVariants({ variant, size }), className)}
      {...props}
    />
  );
}

export { Button, buttonVariants };

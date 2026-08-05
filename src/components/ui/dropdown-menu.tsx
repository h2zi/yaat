import { DropdownMenu as DropdownPrimitive } from "radix-ui";
import { Check, ChevronRight } from "lucide-react";
import type * as React from "react";

import { cn } from "@/lib/utils";

const DropdownMenu = DropdownPrimitive.Root;
const DropdownMenuTrigger = DropdownPrimitive.Trigger;
const DropdownMenuGroup = DropdownPrimitive.Group;
const DropdownMenuPortal = DropdownPrimitive.Portal;
const DropdownMenuSub = DropdownPrimitive.Sub;
const DropdownMenuRadioGroup = DropdownPrimitive.RadioGroup;

function DropdownMenuContent({
  className,
  sideOffset = 6,
  ...props
}: React.ComponentProps<typeof DropdownPrimitive.Content>) {
  return (
    <DropdownPrimitive.Portal>
      <DropdownPrimitive.Content
        sideOffset={sideOffset}
        className={cn(
          "z-[70] min-w-40 overflow-hidden rounded-xl border border-border bg-popover p-1.5 text-popover-foreground shadow-menu data-[state=closed]:animate-out data-[state=open]:animate-in data-[state=closed]:fade-out data-[state=open]:fade-in data-[state=closed]:zoom-out-95 data-[state=open]:zoom-in-95",
          className,
        )}
        {...props}
      />
    </DropdownPrimitive.Portal>
  );
}

function DropdownMenuItem({
  className,
  inset,
  destructive,
  ...props
}: React.ComponentProps<typeof DropdownPrimitive.Item> & {
  inset?: boolean;
  destructive?: boolean;
}) {
  return (
    <DropdownPrimitive.Item
      className={cn(
        "relative flex cursor-default select-none items-center gap-2 rounded-lg px-2 py-2 text-sm outline-none transition-colors data-[disabled]:pointer-events-none data-[disabled]:opacity-50 data-[highlighted]:bg-muted",
        inset && "pl-8",
        destructive && "text-destructive data-[highlighted]:bg-destructive/10",
        className,
      )}
      {...props}
    />
  );
}

function DropdownMenuCheckboxItem({
  className,
  children,
  checked,
  ...props
}: React.ComponentProps<typeof DropdownPrimitive.CheckboxItem>) {
  return (
    <DropdownPrimitive.CheckboxItem
      className={cn(
        "relative flex cursor-default select-none items-center rounded-lg py-2 pl-8 pr-2 text-sm outline-none data-[highlighted]:bg-muted",
        className,
      )}
      checked={checked}
      {...props}
    >
      <span className="absolute left-2 grid size-4 place-items-center">
        <DropdownPrimitive.ItemIndicator>
          <Check className="size-3.5" />
        </DropdownPrimitive.ItemIndicator>
      </span>
      {children}
    </DropdownPrimitive.CheckboxItem>
  );
}

function DropdownMenuSubTrigger({
  className,
  inset,
  children,
  ...props
}: React.ComponentProps<typeof DropdownPrimitive.SubTrigger> & {
  inset?: boolean;
}) {
  return (
    <DropdownPrimitive.SubTrigger
      className={cn(
        "flex cursor-default select-none items-center rounded-lg px-2 py-2 text-sm outline-none data-[state=open]:bg-muted data-[highlighted]:bg-muted",
        inset && "pl-8",
        className,
      )}
      {...props}
    >
      {children}
      <ChevronRight className="ml-auto size-4" />
    </DropdownPrimitive.SubTrigger>
  );
}

function DropdownMenuSubContent({
  className,
  ...props
}: React.ComponentProps<typeof DropdownPrimitive.SubContent>) {
  return (
    <DropdownPrimitive.SubContent
      className={cn(
        "z-[70] min-w-36 overflow-hidden rounded-xl border border-border bg-popover p-1.5 shadow-menu",
        className,
      )}
      {...props}
    />
  );
}

function DropdownMenuLabel({
  className,
  inset,
  ...props
}: React.ComponentProps<typeof DropdownPrimitive.Label> & { inset?: boolean }) {
  return (
    <DropdownPrimitive.Label
      className={cn(
        "px-2 py-1.5 text-xs font-semibold text-muted-foreground",
        inset && "pl-8",
        className,
      )}
      {...props}
    />
  );
}

function DropdownMenuSeparator({
  className,
  ...props
}: React.ComponentProps<typeof DropdownPrimitive.Separator>) {
  return (
    <DropdownPrimitive.Separator
      className={cn("-mx-1 my-1 h-px bg-border", className)}
      {...props}
    />
  );
}

export {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuCheckboxItem,
  DropdownMenuGroup,
  DropdownMenuPortal,
  DropdownMenuSub,
  DropdownMenuSubTrigger,
  DropdownMenuSubContent,
  DropdownMenuRadioGroup,
  DropdownMenuLabel,
  DropdownMenuSeparator,
};

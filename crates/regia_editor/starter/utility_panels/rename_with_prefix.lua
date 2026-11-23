-- The starter utility panel's driver (ADR-8189): "rename" puts the prefix field's text in front of
-- every selected entity's name. The calls run as one undo step.
regia.on("command", function(ctx)
    if ctx.command ~= "rename" then
        return
    end
    local prefix = ctx.values.prefix or ""
    if prefix == "" then
        return
    end
    for _, item in ipairs(ctx.selection) do
        regia.emit("editor.call", {
            method = "editor.outliner_rename",
            params = { entity = item.entity, name = prefix .. item.name },
        })
    end
end)

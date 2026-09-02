return {
  cmd = { "quirl", "lsp" },
  filetypes = { "quirl" },
  on_attach = function(client, bufnr)
    if client:supports_method("textDocument/completion", bufnr) then
      vim.lsp.completion.enable(true, client.id, bufnr, { autotrigger = true })
    end
  end,
}

local plugin = {}

function plugin.deploy(environment)
  return quirl.process.run("deploy " .. environment)
end

return plugin

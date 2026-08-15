--!strict

type Command = {
    program: string,
    args: {string},
    retries: number,
}

local function validate(command: Command): number
    return command.retries + 1
end

local deploy: Command = {
    program = "deploy",
    args = {"--env", "production"},
    retries = "three",
}

validate(deploy)

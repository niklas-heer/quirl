// Bounded excerpt from completers/common/fd_completer/cmd/root.go at
// ebfb9beda84fdf057dc9a89b59527158d23d323c. Retained under the adjacent MIT license.
package cmd

import "github.com/spf13/cobra"

var rootCmd = &cobra.Command{
	Use:   "fd",
	Short: "find entries in the filesystem",
	Long:  "https://github.com/sharkdp/fd",
}

func init() {
	rootCmd.Flags().BoolP("absolute-path", "a", false, "Show absolute instead of relative paths")
	rootCmd.Flags().StringP("exclude", "E", "", "Exclude entries that match the given glob pattern")
	rootCmd.Flags().StringSliceP("search-path", "", nil, "Provide paths to search")

	carapace.Gen(rootCmd).FlagCompletion(carapace.ActionMap{
		"search-path": carapace.ActionDirectories(),
	})

	carapace.Gen(rootCmd).PositionalAnyCompletion(
		carapace.ActionDirectories(),
	)
}

/** @type {import('tailwindcss').Config} */
export default {
  darkMode: ["class"],
  content: ["./index.html", "./src/**/*.{js,ts,jsx,tsx}"],
  theme: {
    extend: {
      colors: {
        doubleword: {
          primary: "#fe6767",
          concrete: "#e2e0d3",
          yellow: "#fcda7e",
          purple: "#6d71f5",
          green: "#76c08f",
          red: {
            50: "#fff5f5",
            100: "#ffe8e8",
            200: "#ffd4d4",
            300: "#ffb3b3",
            400: "#ff8a8a",
            500: "#fe6767",
            600: "#e54d4d",
            700: "#cc3636",
            800: "#a62b2b",
            900: "#802222",
            DEFAULT: "#fe6767",
            light: "#ff8a8a",
            dark: "#e54d4d",
          },
          neutral: {
            50: "#fafaf9",
            100: "#f5f4f1",
            200: "#ebe9e0",
            300: "#e2e0d3",
            400: "#d0cdb8",
            500: "#b5b19a",
            600: "#938f78",
            700: "#726e5b",
            800: "#4f4c3f",
            900: "#2e2c26",
          },
          background: {
            primary: "#ffffff",
            secondary: "#fafaf9",
            tertiary: "#f5f4f1",
            card: "#ffffff",
            dark: "#2e2c26",
          },
          text: {
            primary: "#2e2c26",
            secondary: "#4f4c3f",
            tertiary: "#726e5b",
            muted: "#938f78",
            light: "#ffffff",
            "light-secondary": "#fafaf9",
          },
          border: {
            DEFAULT: "#e2e0d3",
            light: "#f5f4f1",
            dark: "#d0cdb8",
          },
        },
        background: "hsl(var(--background))",
        foreground: "hsl(var(--foreground))",
        card: {
          DEFAULT: "hsl(var(--card))",
          foreground: "hsl(var(--card-foreground))",
        },
        popover: {
          DEFAULT: "hsl(var(--popover))",
          foreground: "hsl(var(--popover-foreground))",
        },
        primary: {
          DEFAULT: "hsl(var(--primary))",
          foreground: "hsl(var(--primary-foreground))",
        },
        secondary: {
          DEFAULT: "hsl(var(--secondary))",
          foreground: "hsl(var(--secondary-foreground))",
        },
        muted: {
          DEFAULT: "hsl(var(--muted))",
          foreground: "hsl(var(--muted-foreground))",
        },
        accent: {
          DEFAULT: "hsl(var(--accent))",
          foreground: "hsl(var(--accent-foreground))",
        },
        destructive: {
          DEFAULT: "hsl(var(--destructive))",
          foreground: "hsl(var(--destructive-foreground))",
        },
        border: "hsl(var(--border))",
        input: "hsl(var(--input))",
        ring: "hsl(var(--ring))",
        chart: {
          1: "hsl(var(--chart-1))",
          2: "hsl(var(--chart-2))",
          3: "hsl(var(--chart-3))",
          4: "hsl(var(--chart-4))",
          5: "hsl(var(--chart-5))",
        },
      },
      fontFamily: {
        "space-grotesk": ['Space Grotesk"', "sans-serif"],
        sans: ['Space Grotesk"', "ui-sans-serif", "system-ui", "sans-serif"],
      },
      fontSize: {
        responsive: "calc(1rem + 0.5vw)",
      },
      borderRadius: {
        xl: "1rem",
        "2xl": "1.5rem",
        "3xl": "2rem",
        "4xl": "3rem",
        lg: "var(--radius)",
        md: "calc(var(--radius) - 2px)",
        sm: "calc(var(--radius) - 4px)",
      },
      animation: {
        "fade-in": "fadeIn 0.3s ease-in-out",
        "slide-up": "slideUp 0.3s ease-out",
      },
      keyframes: {
        fadeIn: {
          "0%": {
            opacity: "0",
          },
          "100%": {
            opacity: "1",
          },
        },
        slideUp: {
          "0%": {
            transform: "translateY(10px)",
            opacity: "0",
          },
          "100%": {
            transform: "translateY(0)",
            opacity: "1",
          },
        },
      },
    },
  },
  plugins: [require("tailwindcss-animate")],
};

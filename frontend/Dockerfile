FROM node:trixie

WORKDIR /app

# Copy package files and install dependencies
COPY package*.json ./
RUN npm install

# Copy the rest of the frontend source code
COPY . .

EXPOSE 5173

# Run Vite in development mode and bind to all interfaces
CMD ["npm", "run", "dev", "--", "--host"]
